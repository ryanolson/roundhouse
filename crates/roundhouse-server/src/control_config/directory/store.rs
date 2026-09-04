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
    /// Which run of the store's counter this version belongs to (R-D2″).
    /// Opaque: compared for equality against the lineage this node claimed,
    /// and never anything else. See [`DirectoryStore`]'s identity rule.
    pub lineage: String,
    /// What the writer of *this version* said it compiled against. Compared
    /// against [`DirectoryStore::compiled_under`] by the reader; see
    /// [`ControlDirectory::divergence`](super::ControlDirectory::divergence).
    pub compiled_under: CompiledUnder,
}

/// What version a store stands at, and which run of its counter that version
/// belongs to (R-D2″).
///
/// The pair rather than a number, because a number alone cannot answer the
/// question the reader is actually asking — *has the document I compiled been
/// replaced* — for a store that can lose its key and start counting again. See
/// [`DirectoryStore`]'s identity rule for why that is a real deployment event
/// rather than a hypothetical, and
/// [`ControlDirectory::plane`](super::ControlDirectory::plane) for what a
/// reader does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    pub lineage: String,
    pub version: u64,
}

impl StoredVersion {
    /// Whether this identity is a **later state of the same document** than
    /// the one a node is serving — the publish rule, in one place.
    ///
    /// Both the refresh and the write path decide whether to install what they
    /// are holding, and they have to decide it the same way: a second copy of
    /// this comparison that drifted would let one of the two install a plane
    /// the other had already moved past. Two halves, and the second is the
    /// M16.1 review's F1: a higher number only means "later" *within* a
    /// lineage, so a document from a run of the counter this node has already
    /// moved off is not newer however it is numbered.
    ///
    /// Version zero is the exception, and not a special case bolted on: a node
    /// that has never seen a document has no lineage to be the same as, so the
    /// first document a deployment ever writes is adopted whichever lineage
    /// minted it. Without that, a node booted against an empty store would
    /// refuse the very first write for the life of the process.
    pub fn supersedes(&self, served: &StoredVersion) -> bool {
        self.version > served.version && (served.version == 0 || self.lineage == served.lineage)
    }
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
    /// The encoded document is over
    /// [`DIRECTORY_DOCUMENT_CEILING_BYTES`](super::document::DIRECTORY_DOCUMENT_CEILING_BYTES).
    ///
    /// **Named rather than folded into [`Self::Unavailable`] (M18, H2).**
    /// `DocumentDirectoryStore::commit` used to answer a ceiling breach with
    /// `Unavailable`, which is the same variant a dead Redis answers -- so
    /// over HTTP a client-caused refusal and a dependency outage rendered as
    /// the identical `directory_unavailable` `500`, and an operator watching
    /// error rates could not tell "our own tenancy grew past a size this
    /// deployment ships with" from "the store is down" without reading the
    /// message. `size` and `ceiling` travel on the type, not only in the
    /// message, so `http.rs`'s mapping can answer a `413` naming both without
    /// re-parsing English out of a string.
    #[error(
        "the directory document is {size} bytes, over this family's {ceiling}-byte ceiling; \
         refusing to write it rather than risk a store's response budget sized for a document \
         at or under that ceiling"
    )]
    DocumentTooLarge { size: usize, ceiling: usize },
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
/// # A document's identity is (lineage, version) (R-D2′, completed as R-D2″)
///
/// **Within one lineage, an implementation's version only ever goes up.** Every
/// [`commit`] returns a version strictly greater than any version this store
/// has previously returned from [`commit`] or answered from [`version`] in that
/// lineage, and [`version`] never answers something lower than it answered
/// before. A store that breaks that has *regressed* — restored from a backup,
/// flushed, or failed over to a replica that had not caught up — and a
/// regression is not something the callers above can prevent; it is something
/// they have to be able to name.
///
/// **The lineage is the half that makes the promise keepable**, and it was
/// added because the number alone was not (M16.1 review, F1). A durable
/// store's counter lives in the store: delete the key, flush it, restore an
/// older snapshot, and the next commit hands out version 1 again — a version a
/// node is *already serving*. Nothing about that number is lower than what
/// that node claimed, so a check that compared numbers would see a quiet
/// deployment and go on serving a plane the deployment has replaced,
/// revocations included. So the store answers which run of the counter it is
/// on, and a reader compares the pair: same lineage and same version is
/// genuinely unchanged, a different lineage is a regression however the
/// numbers compare.
///
/// This is stated here rather than discovered per backend because the whole
/// refresh rung turns on it: [`ControlDirectory::plane`] publishes by comparing
/// identities, so under a store whose numbers can repeat, "newer wins"
/// silently means "the store's current truth loses, forever". The requirement
/// is inherited by [`DocumentStore`](roundhouse_core::control::DocumentStore),
/// which is where both backends satisfy it: `MemoryDocumentStore`
/// structurally, with one counter only ever incremented under one lineage
/// minted at construction, and the Redis store deliberately, with a script
/// that writes the version, the lineage and the document they describe
/// together and mints a new lineage exactly when it finds a key holding none.
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
    /// `expected_version`. Returns the identity the store now stands at, whose
    /// version is strictly greater than every version this store has handed
    /// out in that lineage — see the trait doc's identity rule.
    ///
    /// The identity rather than the number, because a writer whose commit
    /// *started* a lineage — the first write of a deployment's life, or the
    /// first after the key was lost — has no other way to learn which lineage
    /// it just published into, and would have to guess or re-read.
    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<StoredVersion, StoreFailure>;

    /// The current identity, without paying to read the records.
    ///
    /// The cheap half of [`ControlDirectory::plane`]'s refresh: a node past its
    /// TTL asks this first and recompiles only if the answer moved, so a quiet
    /// deployment costs one small read per TTL rather than a compile.
    ///
    /// Within a lineage, never lower than an answer this store has already
    /// given — the trait doc's identity rule, which is what lets a caller read
    /// "lower than last time" as a regression rather than as an ordinary
    /// write, and "a different lineage" as one however the numbers compare.
    async fn version(&self) -> Result<StoredVersion, StoreFailure>;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(lineage: &str, version: u64) -> StoredVersion {
        StoredVersion {
            lineage: lineage.to_string(),
            version,
        }
    }

    /// The publish rule, including the case no fixture in this workspace can
    /// reach through a store (M16.1 review, F1).
    ///
    /// The first three lines are what "newer wins" always meant. The last two
    /// are the ones a test has to hold, because nothing else does: only a
    /// durable backend can answer *no lineage at all* — its key does not exist
    /// yet, so there is nothing to mint one from — and `MemoryDocumentStore`
    /// mints one at construction and answers it from version zero on. So a
    /// node booted against an empty Redis serves `("", 0)`, and a rule that
    /// asked for a matching lineage would refuse the deployment's very first
    /// write, for the life of the process, with every in-memory fixture in the
    /// suite still green.
    #[test]
    fn later_means_later_in_the_same_lineage_and_anything_beats_version_zero() {
        assert!(at("one", 2).supersedes(&at("one", 1)));
        assert!(!at("one", 1).supersedes(&at("one", 2)));
        assert!(!at("one", 1).supersedes(&at("one", 1)));

        assert!(
            !at("two", 9).supersedes(&at("one", 1)),
            "a document from a run of the counter this node has moved off is \
             not newer however it is numbered -- the whole of the identity rule"
        );

        assert!(
            at("one", 1).supersedes(&at("", 0)),
            "a node that has never seen a document has no lineage to match, so \
             the first write of a deployment's life is adopted -- a durable \
             store's empty key is exactly this shape"
        );
        assert!(
            at("one", 1).supersedes(&at("two", 0)),
            "and the same holds whatever a backend chooses to answer at \
             version zero, since version zero means no document"
        );
    }
}
