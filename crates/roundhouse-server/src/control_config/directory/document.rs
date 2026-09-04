// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The typed directory over an opaque document: where the records become bytes
//! and back again.
//!
//! # R-D7 — the serde lives here, and nowhere else
//!
//! [`DocumentStore`] knows nothing about a project or a key: it holds one
//! versioned `Vec<u8>` and answers whether a write raced another one (R-D5).
//! This module is the only place in the workspace that turns
//! [`DirectoryRecords`] into those bytes, so the storage crate never sees this
//! crate's config vocabulary and this crate never sees a Redis key. Every
//! backend the boot can choose — the Redis document store, a
//! `MemoryDocumentStore` — arrives at the directory through exactly this
//! adapter, which is why *every* directory test runs the round trip rather
//! than one test that remembers to.
//!
//! # The envelope, and why it has three fields rather than one
//!
//! ```json
//! { "schema": 1, "records": { … }, "compiled_under": { … } }
//! ```
//!
//! - **`schema`** is the compatibility gate, and the reason it is a number
//!   rather than a version string is that only one comparison is ever made
//!   against it: is this document newer than this build understands. A
//!   document at a *higher* schema is refused at load with a typed reason
//!   (fail closed). The alternative — parse what we recognise and ignore the
//!   rest — is worse than a stopped boot in the specific way that matters
//!   here: a plane compiled from half a document admits and refuses the wrong
//!   callers, silently, and the deployment has no way to notice. A document at
//!   the same schema or lower is read normally, because every field these rows
//!   have gained since is `#[serde(default)]` (see [`records`]).
//! - **`records`** is the tenancy itself.
//! - **`compiled_under`** is the writer's fingerprint of the inputs it
//!   compiled against — the file, the catalog, the fleet's routing candidates
//!   and the TTL. Written here, on every commit, so that a reader whose own
//!   inputs differ can say so (R-D9). This module *stamps* and *carries* it;
//!   what a reader does about a difference is decided one level up, where the
//!   plane being served is.
//!
//! The envelope struct is deliberately **not** `deny_unknown_fields`: a future
//! build adding a fourth top-level key must not break the older half of a
//! fleet during a rolling upgrade, and an envelope key an older build does not
//! recognise is by construction something it does not need. That is the whole
//! of "same-schema unknown fields are tolerated".
//!
//! **The rows inside keep the file vocabulary's `deny_unknown_fields`, and
//! that is a deliberate asymmetry rather than an oversight.** `ProjectEntry`
//! and friends are strict because a mistyped key in an operator's *file*
//! silently widens a policy, and that reasoning is untouched by this rung —
//! the entries in the document are the same entries, and softening them to
//! serve a storage concern would give the file back the failure mode
//! `deny_unknown_fields` exists to prevent. The consequence is stated rather
//! than hidden: a build that adds a field to a config entry has changed what a
//! stored document can contain, so it bumps `schema`, and an older node then
//! refuses that document by name instead of quietly dropping the field.
//!
//! # What is not here
//!
//! No compile, no validation, no cross-checks. This adapter is a codec and a
//! version passthrough; the judgement that decides whether a new set of
//! records is legal happens above it, between the `load` and the `commit`, for
//! the reason [`DirectoryStore`] gives.
//!
//! [`records`]: super::records

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use roundhouse_core::control::directory::{DocumentStore, DocumentStoreError};

use super::records::DirectoryRecords;
use super::store::{DirectoryStore, StoreFailure, StoredVersion, VersionedRecords};

/// The schema this build writes, and the highest it will read.
///
/// One number for the whole envelope, bumped when a stored document stops
/// being readable by the build before it — which, given the rows keep
/// `deny_unknown_fields`, is any change to the config vocabulary a record
/// wraps. It is *not* bumped for an added optional field on a record type
/// itself: those are `#[serde(default)]`, so both directions still read.
pub const DIRECTORY_DOCUMENT_SCHEMA: u32 = 1;

/// The largest encoded document this adapter will hand to a store (M16.1
/// review, F6).
///
/// **8 MiB, because the contract this family actually promises is small.**
/// The directory's own module doc puts the whole deployment's tenancy at "a
/// few thousand keys", and the fully populated fixture this module's tests
/// pin (`every_field_populated`) encodes at roughly 330 bytes per key record
/// — so even a generous multiple of "a few thousand" (ten thousand keys, a
/// deployment two orders of magnitude past anything this crate has been
/// asked to hold) lands in the *tens* of megabytes at the very most, and a
/// real deployment's document is hundreds of kilobytes. 8 MiB leaves that
/// room without leaving so much that a document large enough to hurt the
/// shared Redis connection ever reaches the wire in the first place.
///
/// **Enforced here, before any store call, rather than left to whatever the
/// backend's own timeout does with an oversized write.** A store's response
/// budget is sized to carry a document up to some size with margin — see
/// `DIRECTORY_RESPONSE_TIMEOUT` in `roundhouse-store-redis` — but a budget is
/// a number chosen for *today's* ceiling, and a document that grew past it
/// used to fail as a bare timeout indistinguishable from Redis being down,
/// with no line anywhere naming the actual problem. A refusal here names both
/// numbers and happens before the write is ever attempted, so growing past
/// the ceiling is a refusal an operator can read rather than an outage they
/// have to diagnose.
pub const DIRECTORY_DOCUMENT_CEILING_BYTES: usize = 8 * 1024 * 1024;

/// What the writer compiled its plane from, as a fingerprint (R-D9).
///
/// Stamped into every document this build commits and carried back out of
/// every document it loads, so a node can compare the inputs *it* has against
/// the inputs the document was written under. A difference is not an error —
/// a rolling file change makes divergence the ordinary state for a few seconds
/// — which is why nothing here refuses anything; it is a fact a reader can
/// name.
///
/// Every field is `#[serde(default)]` and every field is written even when
/// empty. The first is what lets a document written by a build that had no
/// fingerprint (or a smaller one) still load; the second keeps the envelope's
/// shape stable, so the byte-for-byte fixture pins a document rather than a
/// coincidence of which fields happened to be populated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledUnder {
    /// SHA-256 of the control-plane file's bytes, hex.
    ///
    /// The bytes and not the parsed config: two files that parse to the same
    /// configuration are the same deployment intent, and a reader comparing
    /// hashes wants to know whether the *document* it is looking at came from
    /// the file it is holding. `None` for a deployment with no file.
    #[serde(default)]
    pub file_sha256: Option<String>,
    /// The catalog's `(provider, model)` identities, sorted.
    #[serde(default)]
    pub catalog: Vec<String>,
    /// The routing-candidate identities the cross-checks were built from,
    /// sorted.
    #[serde(default)]
    pub fleet: Vec<String>,
    /// The admission-cache TTL in force when this document was written.
    #[serde(default)]
    pub admission_cache_ttl_ms: Option<u64>,
}

impl CompiledUnder {
    /// Which of the four inputs this fingerprint and `other` disagree about
    /// (R-D9), in a fixed order, empty when they agree.
    ///
    /// A list rather than a `bool`, because the four axes have four different
    /// remedies: a file that differs is a rolling config change (or a node
    /// pointed at the wrong file), a catalog that differs is a node priced
    /// against models its neighbours do not have, a fleet that differs is a
    /// node whose cross-checks would refuse a plane its neighbours accept, and
    /// a TTL that differs is only a disagreement about how long a revocation
    /// may take. An operator told "the directory diverges" learns nothing;
    /// told *which* input, they know where to look.
    ///
    /// Order is declaration order and is stable, so a test may pin the vector
    /// rather than sorting it — and so two nodes reporting the same divergence
    /// report it identically.
    pub fn differs_from(&self, other: &CompiledUnder) -> Vec<DivergentInput> {
        let mut differs = Vec::new();
        if self.file_sha256 != other.file_sha256 {
            differs.push(DivergentInput::File);
        }
        if self.catalog != other.catalog {
            differs.push(DivergentInput::Catalog);
        }
        if self.fleet != other.fleet {
            differs.push(DivergentInput::Fleet);
        }
        if self.admission_cache_ttl_ms != other.admission_cache_ttl_ms {
            differs.push(DivergentInput::Ttl);
        }
        differs
    }
}

/// One of the four inputs a stored document's writer and its reader can
/// disagree about.
///
/// Named rather than reported as a diff of two fingerprints, because the two
/// fingerprints are large (a catalog is every model this deployment prices)
/// and the actionable part is small. A `Vec<DivergentInput>` fits in one log
/// line and in one assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DivergentInput {
    /// `ROUNDHOUSE_CONTROL_PLANE`'s bytes.
    File,
    /// The catalog's `(provider, model)` identities.
    Catalog,
    /// The routing candidates the cross-checks were built from.
    Fleet,
    /// `admission_cache_ttl_ms`.
    Ttl,
}

impl DivergentInput {
    /// The word a log line and a test use for this axis.
    pub fn as_str(self) -> &'static str {
        match self {
            DivergentInput::File => "file",
            DivergentInput::Catalog => "catalog",
            DivergentInput::Fleet => "fleet",
            DivergentInput::Ttl => "admission_cache_ttl_ms",
        }
    }
}

/// A stored version whose writer compiled it against inputs this node does not
/// have (R-D9).
///
/// **A fact, never a refusal.** A rolling config change makes divergence the
/// ordinary state of a fleet for as long as the rollout takes, and a node that
/// refused to serve a directory written by a neighbour one config ahead would
/// turn every deployment into an outage. What the node does is compile the
/// plane from the inputs *it* holds — the only inputs it can honestly compile
/// against — and say, once, which of them the writer did not share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryDivergence {
    /// The stored version whose fingerprint differed.
    pub version: u64,
    /// Which inputs differed, in [`CompiledUnder::differs_from`]'s order.
    pub differs: Vec<DivergentInput>,
}

/// The envelope, as it is written and read.
///
/// Field order here is the field order in the JSON — `serde_json` writes a
/// struct in declaration order — which is what makes the byte-for-byte fixture
/// in this module's tests a pin rather than a coincidence.
#[derive(Debug, Serialize, Deserialize)]
struct DirectoryDocument {
    schema: u32,
    records: DirectoryRecords,
    /// `#[serde(default)]` so a document written before the fingerprint
    /// existed still loads, as a fingerprint that matches nothing.
    #[serde(default)]
    compiled_under: CompiledUnder,
}

/// Just the schema number, read before anything else.
///
/// A separate, permissive shape rather than reading `schema` off the parsed
/// envelope, and the ordering is the point: a document one schema ahead very
/// likely *also* fails to parse as records this build knows, and the reason a
/// caller can act on is "this document is newer than this build" rather than
/// whatever serde says about the first unfamiliar field it met. So the schema
/// is checked first and answers first.
#[derive(Deserialize)]
struct SchemaProbe {
    /// No default: a document with no `schema` at all is not one this
    /// deployment wrote, and reading it as schema 1 would be guessing about
    /// the one field whose whole job is to stop guessing.
    schema: u32,
}

/// [`DirectoryStore`] over any [`DocumentStore`] — the only implementation
/// this deployment ships.
///
/// Holds the fingerprint it stamps on every write. The fingerprint is fixed
/// for the life of the handle because the inputs it describes are: the file is
/// read once at boot, the catalog and the fleet's candidates are what this
/// process was built with. A node whose inputs change gets a new process.
pub struct DocumentDirectoryStore {
    store: Arc<dyn DocumentStore>,
    compiled_under: CompiledUnder,
}

impl DocumentDirectoryStore {
    /// Adapt a document store, stamping an empty fingerprint.
    ///
    /// The constructor every fixture uses, and the one a deployment with
    /// nothing to fingerprint (no file, no catalog) uses too. An empty
    /// fingerprint is honest rather than absent: it says "this writer declared
    /// no inputs", which is exactly what a reader comparing against its own
    /// inputs should see.
    pub fn over(store: Arc<dyn DocumentStore>) -> Self {
        Self {
            store,
            compiled_under: CompiledUnder::default(),
        }
    }

    /// Adapt a document store, stamping `compiled_under` on every commit.
    ///
    /// What the composition root calls once it has hashed the file and
    /// enumerated the catalog and the fleet (R-D9).
    pub fn stamped(store: Arc<dyn DocumentStore>, compiled_under: CompiledUnder) -> Self {
        Self {
            store,
            compiled_under,
        }
    }

    /// The fingerprint this handle stamps — what a reader compares the loaded
    /// one against.
    pub fn compiled_under(&self) -> &CompiledUnder {
        &self.compiled_under
    }
}

#[async_trait]
impl DirectoryStore for DocumentDirectoryStore {
    /// One round trip, keeping the writer's fingerprint — see
    /// [`VersionedRecords::compiled_under`].
    async fn load(&self) -> Result<VersionedRecords, StoreFailure> {
        let loaded = self.store.load().await.map_err(failure)?;
        match loaded.document {
            None if loaded.version == 0 => Ok(VersionedRecords {
                records: DirectoryRecords::default(),
                version: 0,
                lineage: loaded.lineage,
                compiled_under: CompiledUnder::default(),
            }),
            // The mirror of the arm below, and refused for the mirror reason
            // (M16.1 review, F4). Version zero *is* "no document" by contract,
            // so a store answering a document at version zero is answering two
            // things that cannot both be true -- and the one it can only be is
            // a durable key whose version field is gone: a foreign writer, an
            // `HDEL`, a partial restore. Compiling that document would be
            // compiling tenancy whose version this deployment never observed,
            // and the very next admin write would `commit(0, ..)` straight
            // over it. `MemoryDocumentStore` cannot produce the shape at all,
            // which is exactly why the refusal belongs here rather than in one
            // backend.
            Some(_) if loaded.version == 0 => Err(StoreFailure::Unavailable(
                "the directory store is at version 0 -- the empty directory -- and holds a \
                 document; refusing to compile a plane from a document whose version this \
                 node never read"
                    .to_string(),
            )),
            // A store that says it has been written and holds nothing has lost
            // the document -- a hand-edited key, a partial restore. Reading it
            // as the empty directory would silently un-configure every project
            // the API ever created, and the next admin write would commit that
            // emptiness over the top; refusing lets the boot say where to look
            // and lets a running node keep the plane it already serves.
            None => Err(StoreFailure::Unavailable(format!(
                "the directory store is at version {} and holds no document; refusing to read \
                 that as an empty directory",
                loaded.version
            ))),
            Some(bytes) => {
                let document = decode(&bytes)?;
                Ok(VersionedRecords {
                    records: document.records,
                    version: loaded.version,
                    lineage: loaded.lineage,
                    compiled_under: document.compiled_under,
                })
            }
        }
    }

    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<StoredVersion, StoreFailure> {
        let document = DirectoryDocument {
            schema: DIRECTORY_DOCUMENT_SCHEMA,
            records,
            compiled_under: self.compiled_under.clone(),
        };
        // Mapped rather than unwrapped. `serde_json` refuses a non-finite
        // float, and these records carry operator-supplied dollar amounts --
        // which `ControlPlaneConfig::validate` refuses before any mutation
        // reaches a store, so this arm is unreachable through the admin plane.
        // It is still an error and not a panic, because "unreachable through
        // the admin plane" is a claim about today's callers and a panic here
        // would take the process down for a bad number.
        let bytes = serde_json::to_vec(&document).map_err(|error| {
            StoreFailure::Unavailable(format!(
                "the directory records could not be encoded as a document: {error}"
            ))
        })?;
        // Refused here rather than handed to the store, so growing past the
        // ceiling is a named refusal and not a timeout the caller has to
        // guess the cause of (M16.1 review, F6).
        if bytes.len() > DIRECTORY_DOCUMENT_CEILING_BYTES {
            return Err(StoreFailure::Unavailable(format!(
                "the directory document is {} bytes, over this family's \
                 {DIRECTORY_DOCUMENT_CEILING_BYTES}-byte ceiling; refusing to write it rather \
                 than risk a store's response budget sized for a document at or under that \
                 ceiling",
                bytes.len()
            )));
        }
        self.store
            .commit(expected_version, bytes)
            .await
            .map(identity)
            .map_err(failure)
    }

    async fn version(&self) -> Result<StoredVersion, StoreFailure> {
        self.store.version().await.map(identity).map_err(failure)
    }

    /// What this node compiles against — the stamp [`Self::stamped`] was
    /// built with, which is also what every commit through this handle writes.
    fn compiled_under(&self) -> CompiledUnder {
        self.compiled_under.clone()
    }
}

/// The two answers a document store can give, mapped one for one onto the two
/// this seam already had.
///
/// One for one and not through a catch-all: `Concurrent` is a caller's cue to
/// re-read and retry, and `Unavailable` is not, so a mapping that flattened
/// them would turn every lost race into an outage as far as the HTTP surface
/// is concerned (`409` versus `503`, in `http.rs`).
fn failure(error: DocumentStoreError) -> StoreFailure {
    match error {
        DocumentStoreError::Concurrent { expected, found } => {
            StoreFailure::Concurrent { expected, found }
        }
        DocumentStoreError::Unavailable(reason) => StoreFailure::Unavailable(reason),
    }
}

/// The document store's identity, in this seam's vocabulary — one field for
/// one field, the way [`failure`] maps the errors.
fn identity(version: roundhouse_core::control::DocumentVersion) -> StoredVersion {
    StoredVersion {
        lineage: version.lineage,
        version: version.version,
    }
}

/// Read a stored document: the schema first, then the records.
fn decode(bytes: &[u8]) -> Result<DirectoryDocument, StoreFailure> {
    let probe: SchemaProbe = serde_json::from_slice(bytes).map_err(|error| {
        StoreFailure::Unavailable(format!(
            "the stored directory document could not be read ({error}); refusing to compile a \
             plane from a document this build does not recognise"
        ))
    })?;
    if probe.schema > DIRECTORY_DOCUMENT_SCHEMA {
        return Err(StoreFailure::Unavailable(format!(
            "the stored directory document is schema {} and this build reads up to schema \
             {DIRECTORY_DOCUMENT_SCHEMA}; refusing to compile a plane from a document written \
             by a newer build -- upgrade this node, or point it at a directory it can read",
            probe.schema
        )));
    }
    serde_json::from_slice(bytes).map_err(|error| {
        StoreFailure::Unavailable(format!(
            "the stored directory document is schema {} but could not be read ({error})",
            probe.schema
        ))
    })
}

#[cfg(test)]
mod tests;
