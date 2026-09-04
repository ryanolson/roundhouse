// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where admin-created tenancy lives, and the one in-memory implementation.

use std::sync::Mutex;

use async_trait::async_trait;

use super::records::DirectoryRecords;

/// The records as they stood, and the version they stood at.
///
/// The pair travels together because a write is only safe against the version
/// it read — see [`DirectoryStore::commit`].
#[derive(Debug, Clone)]
pub struct VersionedRecords {
    pub records: DirectoryRecords,
    pub version: u64,
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
/// The three were synchronous for as long as [`MemoryDirectoryStore`] was the
/// only implementation, and a clone of a `Vec` behind a `Mutex` is not
/// something a caller can usefully await. A durable store makes each of them a
/// network round trip, and a synchronous trait leaves exactly two ways to call
/// one from the async surfaces above: block a runtime worker on it, or hand it
/// to `spawn_blocking` and pay a thread per admission. Both are worse than the
/// honest shape, and neither could be swapped for it later without touching
/// every caller anyway — so the seam moves once, here, while the only
/// implementation is still the in-memory one and nothing about the move can be
/// blamed on a backend that does not exist yet.
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
/// means "the store's current truth loses, forever". The in-memory
/// implementation below satisfies the requirement structurally — one counter,
/// only ever incremented — and a durable one must arrange it deliberately.
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
}

/// The single-node store: a `Mutex` over the records and a counter.
///
/// Version 0 is the empty directory, and every successful commit increments —
/// so "has anything changed since I looked" is one integer comparison, which is
/// what [`DirectoryStore::version`] exists to be cheap for.
#[derive(Debug, Default)]
pub struct MemoryDirectoryStore {
    state: Mutex<(DirectoryRecords, u64)>,
}

impl MemoryDirectoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DirectoryStore for MemoryDirectoryStore {
    async fn load(&self) -> Result<VersionedRecords, StoreFailure> {
        // Poisoning is recovered rather than propagated, as everywhere else in
        // this process that holds a `std` lock over plain data: these are
        // records, not an invariant another thread's panic could have left
        // half-built, and refusing every admission for the life of the process
        // because one request panicked is the worse failure.
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(VersionedRecords {
            records: state.0.clone(),
            version: state.1,
        })
    }

    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<u64, StoreFailure> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.1 != expected_version {
            return Err(StoreFailure::Concurrent {
                expected: expected_version,
                found: state.1,
            });
        }
        state.0 = records;
        state.1 += 1;
        Ok(state.1)
    }

    async fn version(&self) -> Result<u64, StoreFailure> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(state.1)
    }
}

#[cfg(test)]
mod tests {
    //! [`MemoryDirectoryStore`]'s own compare-and-set, direct.
    //!
    //! Every `Concurrent` path exercised elsewhere in this crate
    //! (`WriteBetweenReads`, `ScriptedStore`, `ArmedStore` in
    //! `tests/admin_api.rs`) is a hand-rolled double with its own
    //! independently-written `commit`, and none of them delegate to this type
    //! -- so the one production `DirectoryStore` this deployment actually
    //! ships had never had its own CAS check driven by a test. A `commit`
    //! that silently dropped the `expected_version` guard read every existing
    //! suite as green, because nothing asked this store, directly, to refuse
    //! a stale write.

    use super::*;

    #[tokio::test]
    async fn commit_refuses_a_stale_expected_version() {
        let store = MemoryDirectoryStore::new();
        assert_eq!(
            store.version().await.unwrap(),
            0,
            "a fresh store is version 0"
        );

        let first = store
            .commit(0, DirectoryRecords::default())
            .await
            .expect("the store is still at the version this write read");
        assert_eq!(first, 1, "the first successful commit is version 1");

        // The version this store is actually at has moved to 1. A second
        // write that still thinks it is 0 -- the shape of a second node that
        // read before the first node's commit landed -- must be refused
        // rather than silently accepted and overwritten.
        let stale = store.commit(0, DirectoryRecords::default()).await;
        assert!(
            matches!(
                stale,
                Err(StoreFailure::Concurrent {
                    expected: 0,
                    found: 1,
                })
            ),
            "a commit against a version the store has moved past must answer \
             `Concurrent`, naming both the version it expected and the one \
             actually found: {stale:?}"
        );
        // And the refused write changed nothing: the store is still at the
        // version its own successful commit left it at.
        assert_eq!(
            store.version().await.unwrap(),
            1,
            "a refused commit must not have advanced the store"
        );

        // CONTROL: a commit against the version the store is genuinely at
        // succeeds and advances it -- so the refusal above is about the
        // version comparison and not about this store refusing every write.
        let second = store
            .commit(1, DirectoryRecords::default())
            .await
            .expect("a commit against the current version succeeds");
        assert_eq!(second, 2);
    }
}
