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
//! # A document's identity is (lineage, version), not version alone (R-D2″)
//!
//! Inherited from the seam above (`DirectoryStore` in `roundhouse-server`) and
//! completed here by the M16.1 review's F1. **Within one lineage**
//! [`DocumentStore::commit`] returns a version strictly greater than any this
//! store has previously returned from `commit` or answered from
//! [`DocumentStore::version`], `version` never answers lower than it answered
//! before, and a version is never handed out twice. **Including a commit of
//! bytes identical to the ones already stored** — the version is a *write*
//! counter and not a content hash, because the caller above compares versions
//! to decide whether to recompile, and a store that declined to advance on an
//! identical write would silently make an idempotent-looking admin call
//! unobservable to every other node.
//!
//! The lineage is what makes that promise keepable by a durable backend, and
//! it exists because the counter alone could not keep it. A store whose key is
//! deleted, flushed or restored from an older backup has no memory of the
//! versions it handed out, so its next commit starts at 1 again — a version
//! some node is *already serving*, which a reader comparing numbers cannot
//! tell from a deployment where nothing has changed. That is the ABA the
//! regression check one crate up would otherwise miss entirely: not a version
//! that went down, but one that came back around. So a store that loses its
//! key **changes lineage** rather than restarting the counter silently, and
//! the reader compares the pair.
//!
//! The lineage is opaque — a reader may compare two for equality and may do
//! nothing else with one. A store that has never been written carries no
//! lineage claim at all (version zero is the empty store, and there is no
//! document for a lineage to be about); a backend may answer whatever it likes
//! for that case, and the contract only pins the lineage from the first commit
//! on.
//!
//! A store that regresses — a lower version, or a new lineage — has been
//! restored from a backup, flushed, or failed over to a replica that had not
//! caught up. That is not something a caller can prevent, only name — and what
//! the directory does when it happens is decided one crate up, where the plane
//! being served is.
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
    /// Which run of the counter this version belongs to — see the module
    /// doc's identity rule. Meaningless at version zero, where nothing has
    /// been written for a lineage to be about.
    pub lineage: String,
}

/// A document's identity: which run of the counter, and how far along it
/// (R-D2″).
///
/// What [`DocumentStore::version`] and [`DocumentStore::commit`] both answer,
/// as one value rather than two calls, for the reason [`VersionedDocument`]
/// keeps its two fields together: a reader that took the lineage and the
/// version from two round trips could take them from two different states of
/// the store, and the pair is only meaningful read at one instant. It is also
/// why `commit` answers it rather than a bare number — a writer whose commit
/// *started* the lineage (the first write of a deployment's life, or the first
/// after the key was lost) has no other way to learn which lineage it is now
/// in, and would otherwise have to guess or re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentVersion {
    pub lineage: String,
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
    /// `expected_version`. Returns the identity it now stands at: a version
    /// strictly greater than every version this store has handed out in that
    /// lineage — see the module doc.
    async fn commit(
        &self,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<DocumentVersion, DocumentStoreError>;

    /// The current identity, without paying to read the document.
    ///
    /// The cheap half of the caller's refresh: a node past its TTL asks this
    /// first and reads the document only if the answer moved, so a quiet
    /// deployment costs one small read per TTL rather than a whole document.
    /// That is worth a method of its own here in a way it is not for a
    /// `HashMap`-shaped store, because the document is the deployment's entire
    /// tenancy and can be megabytes.
    ///
    /// The lineage rides along rather than being asked for separately: it is
    /// the half of the answer that catches a counter which restarted, and a
    /// reader that had to make a second call for it would be comparing two
    /// halves read at two instants.
    async fn version(&self) -> Result<DocumentVersion, DocumentStoreError>;
}

/// The single-node store: a `Mutex` over the document and a counter, under one
/// lineage.
///
/// Version 0 with no document is the empty store, and every successful commit
/// increments — so the monotone requirement holds structurally here (one
/// counter, only ever incremented) and a durable backend has to arrange it
/// deliberately.
///
/// **The lineage is minted at construction and never changes**, which is the
/// honest answer for a store that lives and dies with its process: this store
/// cannot lose a key and keep answering, so every version it ever hands out
/// belongs to one run of one counter. A *fresh* store is a new lineage, and
/// that is not incidental — it is what lets a fixture express the durable
/// backend's lost-key case (see [`Self::continuing`] for the other half).
#[derive(Debug)]
pub struct MemoryDocumentStore {
    state: Mutex<(Option<Vec<u8>>, u64)>,
    lineage: String,
}

impl Default for MemoryDocumentStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryDocumentStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new((None, 0)),
            lineage: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// A store carrying on a lineage some earlier store started.
    ///
    /// For a fixture that rebuilds a store at some version and means *the same
    /// deployment's key, still there* rather than *the key was replaced*. The
    /// two are exactly what the identity rule exists to tell apart, so a
    /// double that could only ever do the second could not script a neighbour
    /// node's ordinary write at all.
    pub fn continuing(lineage: impl Into<String>) -> Self {
        Self {
            state: Mutex::new((None, 0)),
            lineage: lineage.into(),
        }
    }

    /// The lineage every version from this store belongs to.
    pub fn lineage(&self) -> &str {
        &self.lineage
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
            lineage: self.lineage.clone(),
        })
    }

    async fn commit(
        &self,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<DocumentVersion, DocumentStoreError> {
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
        Ok(DocumentVersion {
            lineage: self.lineage.clone(),
            version: state.1,
        })
    }

    async fn version(&self) -> Result<DocumentVersion, DocumentStoreError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(DocumentVersion {
            lineage: self.lineage.clone(),
            version: state.1,
        })
    }
}
