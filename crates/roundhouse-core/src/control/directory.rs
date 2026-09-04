// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! One versioned document, compare-and-set, and nothing that knows what is in
//! it.
//!
//! # R-D5 — the contract is a versioned *opaque* document
//!
//! The admin directory — projects, users, memberships and keys — is a
//! `roundhouse-server` vocabulary, and it stays there: what lands here is the
//! one thing a second node needs and a `HashMap` cannot give, which is a
//! durable value with a version a writer can gate on. So [`DocumentStore`]
//! takes and returns `Vec<u8>` and has no opinion about the bytes.
//!
//! **Opaque, and that is the ruling rather than a shortcut.** The alternative
//! — a `DirectoryStore` in the server crate with a Redis implementation beside
//! it — was rejected on where the code would then have to live: the Redis key
//! builder, the namespace type and the connection manager are all in
//! `roundhouse-store-redis`, so a typed store would either duplicate the key
//! format in a third crate (the one thing `keys::build_key` exists to prevent)
//! or drag the server crate's config vocabulary down into the store crate,
//! where a `serde` change to a project entry would be a change to the storage
//! crate's public surface. An opaque document puts the serde at the *server's*
//! boundary, where the vocabulary already is, and leaves the store crate with
//! the only question it is qualified to answer: did this write race another
//! one.
//!
//! # The version is monotone, and every commit advances it
//!
//! Inherited word for word from the seam above (`DirectoryStore`'s R-D2′ in
//! `roundhouse-server`): [`DocumentStore::commit`] returns a version strictly
//! greater than any this store has previously returned from `commit` or
//! answered from [`DocumentStore::version`], and `version` never answers lower
//! than it answered before. **Including a commit of bytes identical to the
//! ones already stored** — the version is a *write* counter and not a content
//! hash, because the caller above compares versions to decide whether to
//! recompile, and a store that declined to advance on an identical write would
//! silently make an idempotent-looking admin call unobservable to every other
//! node.
//!
//! A store that regresses has been restored from a backup, flushed, or failed
//! over to a replica that had not caught up. That is not something a caller
//! can prevent, only name — and what the directory does when it happens is
//! decided one crate up, where the plane being served is.
//!
//! # [`MemoryDocumentStore`] is the specification
//!
//! Exactly as [`MemoryCorrelationMaps`](super::correlation::MemoryCorrelationMaps)
//! and the two ledgers are: `contract` holds the behavioural assertions as one
//! list and `document_store_contract_suite!` instantiates the whole list
//! against a backend in one call. It matters here
//! for the reason it matters there and one more of its own: the Redis
//! implementation's compare-and-set is written in Lua and this one in Rust, so
//! "a stale commit changes nothing" is a claim about both or it is not a claim
//! at all — and unlike the ledgers, this family has exactly one key, so a
//! backend that got the CAS wrong would not lose one tenant's row, it would
//! lose the deployment's tenancy.

#[cfg(any(test, feature = "test-support"))]
pub mod contract;
#[cfg(test)]
mod tests;

use std::sync::Mutex;

use async_trait::async_trait;

/// The document as it stood, and the version it stood at.
///
/// The pair travels together because a write is only safe against the version
/// it read — see [`DocumentStore::commit`].
///
/// `document: None` at `version: 0` is the empty store, and the two facts are
/// deliberately not collapsed into one: a store that had once held a document
/// and been committed *empty bytes* is at some version above zero with
/// `Some(vec![])`, which is a different thing from a deployment that has never
/// written tenancy at all. The caller above reads the first as "there is a
/// document, and it happens to say nothing" and the second as "compile the
/// file alone".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedDocument {
    pub document: Option<Vec<u8>>,
    pub version: u64,
}

/// Why a document store could not answer.
///
/// Two arms, and they are the two the directory's own `StoreFailure` already
/// distinguishes one crate up, so the adapter maps them one for one rather
/// than inventing a third category on the way past. Deliberately nothing about
/// the *content*: a document this store cannot parse is not a thing this store
/// can have an opinion about, because it never parses one.
#[derive(Debug, thiserror::Error)]
pub enum DocumentStoreError {
    #[error(
        "the document moved from version {expected} to {found} while this change was being \
         prepared, so it was not written -- read the current document and try again"
    )]
    Concurrent { expected: u64, found: u64 },
    #[error("the document store is unavailable: {0}")]
    Unavailable(String),
}

/// One versioned document, read and written by compare-and-set.
///
/// # Why `load`/`commit` and not `apply`
///
/// The same reason the typed seam above gives, arriving here intact: the
/// judgement that decides whether a new document is legal — the control-plane
/// compile and the deployment's cross-checks — sits *between* the read and the
/// write, and it is a `roundhouse-server` fact. A store that applied changes
/// internally would have to hold that compiler behind this trait, where a
/// Redis implementation would have to reimplement it.
///
/// `#[async_trait]` rather than a native `async fn`, for
/// [`CorrelationMaps`](super::correlation::CorrelationMaps)' reason: a native
/// `async fn` in a trait is not dyn compatible at this toolchain, and this
/// trait exists to be held as `Arc<dyn DocumentStore>` by a composition root
/// that picked one backend at boot.
#[async_trait]
pub trait DocumentStore: Send + Sync + 'static {
    /// The stored document and the version it was read at.
    async fn load(&self) -> Result<VersionedDocument, DocumentStoreError>;

    /// Replace the document, if and only if the store is still at
    /// `expected_version`. Returns the new version, strictly greater than
    /// every version this store has handed out before — see the module doc.
    async fn commit(
        &self,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<u64, DocumentStoreError>;

    /// The current version, without paying to read the document.
    ///
    /// The cheap half of the caller's refresh: a node past its TTL asks this
    /// first and reads the document only if the answer moved, so a quiet
    /// deployment costs one integer read per TTL rather than a whole document.
    /// That is worth a method of its own here in a way it is not for a
    /// `HashMap`-shaped store, because the document is the deployment's entire
    /// tenancy and can be megabytes.
    async fn version(&self) -> Result<u64, DocumentStoreError>;
}

/// The single-node store: a `Mutex` over the document and a counter.
///
/// Version 0 with no document is the empty store, and every successful commit
/// increments — so the monotone requirement holds structurally here (one
/// counter, only ever incremented) and a durable backend has to arrange it
/// deliberately.
#[derive(Debug, Default)]
pub struct MemoryDocumentStore {
    state: Mutex<(Option<Vec<u8>>, u64)>,
}

impl MemoryDocumentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DocumentStore for MemoryDocumentStore {
    async fn load(&self) -> Result<VersionedDocument, DocumentStoreError> {
        // Poisoning is recovered rather than propagated, as everywhere else
        // that holds a `std` lock over plain data in this workspace: this is a
        // byte string, not an invariant another thread's panic could have left
        // half-built, and refusing every admission for the life of the process
        // because one request panicked is the worse failure.
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(VersionedDocument {
            document: state.0.clone(),
            version: state.1,
        })
    }

    async fn commit(
        &self,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<u64, DocumentStoreError> {
        // The compare and the write MUST stay under this one lock
        // acquisition. Splitting them -- reading `state.1` under one
        // `lock()`, dropping the guard, then reacquiring a second `lock()`
        // to write -- reopens the exact TOCTOU gap this method exists to
        // close, and this crate's own test suite is **not** a mechanical
        // guard against that split (M16.1 review, F2): the contract's
        // `two_commits_racing_against_one_version_admit_exactly_one`
        // (`contract.rs`) races real, barrier-synchronized OS threads rather
        // than `tokio::join!`-ed futures precisely because this method has no
        // `.await` point for a single-task executor to interleave at -- but
        // even genuinely concurrent OS threads were verified (repeatedly, up
        // to 64 racers over 50 rounds) not to expose this exact split:
        // `std::sync::Mutex` on Linux favours a thread that just released a
        // lock over any thread it woke to contend for it, so the split
        // resolves as if the two commits had simply run one after the other,
        // every time. The same harness caught the split immediately once an
        // artificial delay was inserted between its two lock acquisitions,
        // which is what rules out "the test cannot detect a real split" as
        // the explanation -- the window here is just too narrow for
        // wall-clock scheduling to land in. A code reviewer reading this
        // comment is this invariant's actual enforcement until a
        // model-checked test (e.g. `loom`) closes the gap; that is a design
        // decision recorded as open rather than made unilaterally here.
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.1 != expected_version {
            return Err(DocumentStoreError::Concurrent {
                expected: expected_version,
                found: state.1,
            });
        }
        state.0 = Some(document);
        state.1 += 1;
        Ok(state.1)
    }

    async fn version(&self) -> Result<u64, DocumentStoreError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(state.1)
    }
}
