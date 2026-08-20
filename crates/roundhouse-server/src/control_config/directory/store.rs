// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where admin-created tenancy lives, and the one in-memory implementation.

use std::sync::Mutex;

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
pub trait DirectoryStore: Send + Sync {
    /// Every admin-created record, and the version they were read at.
    fn load(&self) -> Result<VersionedRecords, StoreFailure>;

    /// Replace the records, if and only if the store is still at
    /// `expected_version`. Returns the new version.
    fn commit(&self, expected_version: u64, records: DirectoryRecords)
    -> Result<u64, StoreFailure>;

    /// The current version, without paying to read the records.
    ///
    /// The cheap half of [`ControlDirectory::plane`]'s refresh: a node past its
    /// TTL asks this first and recompiles only if the answer moved, so a quiet
    /// deployment costs one version read per TTL rather than a compile.
    fn version(&self) -> Result<u64, StoreFailure>;
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

impl DirectoryStore for MemoryDirectoryStore {
    fn load(&self) -> Result<VersionedRecords, StoreFailure> {
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

    fn commit(
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

    fn version(&self) -> Result<u64, StoreFailure> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(state.1)
    }
}
