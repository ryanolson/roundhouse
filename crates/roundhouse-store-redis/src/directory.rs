// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`DocumentStore`]: the deployment's admin-created tenancy, as
//! one versioned document any node can read and exactly one node at a time can
//! replace.
//!
//! This is what makes the admin plane a deployment surface rather than a
//! single-node one. Until this rung the directory lived in a `Mutex` inside
//! one process, so a project created on node A did not exist on node B, and a
//! restart lost every key the API had ever minted — the state the M8 doc
//! called honest for its milestone and named the unlock condition for.
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `rh:v1:dir:records` | hash | field `version` (decimal) and field `document` (opaque bytes) |
//!
//! # R-D6 — one key, two fields, and no hash tag
//!
//! **One key for the whole deployment**, which is a decision and not an
//! accident of the shape above. The alternative — a key per project, per user,
//! per membership, per key — is what a *typed* store would need, and it is
//! exactly what R-D5 refused: it would put the server crate's config
//! vocabulary in this crate, and it would make "read the directory" a scan
//! whose result is only as consistent as the interleaving it happened to see.
//! The directory above compiles a whole [`ControlPlaneConfig`] from what it
//! reads, so a half-read directory is a plane that admits and refuses the
//! wrong keys. One key is one atomic read.
//!
//! **No hash tag**, unlike every other family here. A tag buys single-slot
//! multi-key atomicity under Redis Cluster, and there is no multi-key
//! operation in this family to buy it for: `load` touches one key, `commit`
//! touches one key, `version` touches one key. Adding a tag would pin this
//! deployment's tenancy to whichever slot the tag hashed to and buy nothing —
//! and, worse, a tag chosen now would be the tag every future operation had to
//! keep, for a family whose whole point is that it has one key.
//!
//! **Two fields rather than two keys**, and that is what makes the
//! compare-and-set expressible at all: the version and the document it
//! describes are written by one `HSET`, so no reader can ever see a version
//! that does not match the bytes beside it. Two keys would need a
//! multi-key script *and* a hash tag to be Cluster-safe, to buy exactly the
//! atomicity one hash already has.
//!
//! # The namespace and the schema version are in the key (R-S3)
//!
//! `rh` is the default [`KeyNamespace`] — a deployment that names its own with
//! `ROUNDHOUSE_REDIS_NAMESPACE` gets that one — and `v1` is this family's own
//! `KeyFamily::version`, built by `keys::build_key` like every other family's
//! key. The version is what makes the *field* encoding below
//! changeable: a v2 that stored the document compressed, or split across
//! fields, would be a different key space rather than a value some v1 node
//! reads as a document and compiles a plane from.
//!
//! # What the script is for, and what it is not
//!
//! Only `commit` is a script. `load` is one `HMGET` and `version` is one
//! `HGET`, because neither has a condition in it. `commit` has exactly one —
//! "the store is still at the version this writer read" — and it is the one
//! condition that must not be evaluated against a value another node is
//! replacing: two nodes that each read version *n* and each wrote would leave
//! one of the two changes silently gone, which for this family means a
//! revocation overwritten by a concurrent rename. A Redis script executes with
//! nothing in between, so the check and the write are one indivisible step.
//!
//! # A field this store did not write fails loudly
//!
//! A `version` field that is not a number is a *foreign writer*, and it fails
//! the read rather than being read as zero — the same rule the correlation
//! maps apply to an untagged binding, and for the sharper version of the same
//! reason: reading it as zero would make the next `commit(0, ..)` succeed and
//! overwrite whatever is there. A store that cannot say what version it is at
//! must refuse, and the boot above turns that refusal into a stopped process
//! with a reason (R-D8).
//!
//! Passes the same `document_store_contract_suite!` that judges
//! [`MemoryDocumentStore`](roundhouse_core::control::MemoryDocumentStore),
//! instantiated ignore-gated in `tests/directory_contract.rs` exactly as
//! `tests/correlation_contract.rs` does for the correlation maps.
//!
//! [`ControlPlaneConfig`]: https://docs.rs/roundhouse-server

mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::directory::{DocumentStore, DocumentStoreError, VersionedDocument};

use crate::keys::{self, KeyNamespace};

/// The hash field holding the write counter, in decimal.
///
/// Named once here and referenced from the script's arguments and from both
/// read paths, rather than typed as a literal at four call sites: a field
/// name that drifted between the script and the reader is a store that
/// commits into one field and loads from another, which reads exactly like an
/// empty directory.
pub(crate) const VERSION_FIELD: &str = "version";

/// The hash field holding the document itself — opaque bytes, never parsed
/// here. See [`VERSION_FIELD`] for why it is a constant.
pub(crate) const DOCUMENT_FIELD: &str = "document";

/// The one key this family occupies.
///
/// No hash tag — see the module doc. `records` rather than a bare family key
/// so a second thing this family ever needs to store (a lock, a marker) has
/// somewhere to go that is not "the key that used to be the whole family".
pub(crate) fn records_key(namespace: &KeyNamespace) -> String {
    keys::build_key(namespace, keys::KeyFamily::Directory, &["records"])
}

fn backend(error: redis::RedisError) -> DocumentStoreError {
    DocumentStoreError::Unavailable(error.to_string())
}

/// The same mapping, naming the key the failed command was about.
///
/// **Because this refusal stops a boot** (R-D8), and the sentence an operator
/// acts on has to say *what to go and look at*. Redis answers `WRONGTYPE` for
/// a directory key some other writer owns, and `WRONGTYPE: Operation against a
/// key holding the wrong kind of value` on its own tells a deployment nothing
/// it can use — there are five families in this Redis and the message names
/// none of them. `decode_version` below already names the key for the failure
/// it catches; this is the same courtesy for the failures Redis itself
/// catches, so both halves of "this key is not ours" read the same way in a
/// boot log.
///
/// Not folded into [`backend`]: `connect_manager` fails before any key is in
/// play, and a connection refusal that named a key would point an operator at
/// a key that was never reached.
fn backend_at(key: &str) -> impl Fn(redis::RedisError) -> DocumentStoreError + '_ {
    move |error| DocumentStoreError::Unavailable(format!("directory key `{key}`: {error}"))
}

/// Read a stored `version` field, refusing anything this store did not write.
///
/// `None` — the field, or the whole key, absent — is version zero: the empty
/// store, which is exactly what [`VersionedDocument`] says version zero means.
/// A present-but-unparseable field is not zero and must never be read as one:
/// zero is the version a first commit expects, so a store that answered zero
/// for a field holding something else would admit that commit and overwrite
/// whatever a foreign writer had put there — turning "I do not understand this
/// key" into a silent clobber.
fn decode_version(raw: Option<String>, key: &str) -> Result<u64, DocumentStoreError> {
    match raw {
        None => Ok(0),
        Some(value) => value.parse::<u64>().map_err(|error| {
            DocumentStoreError::Unavailable(format!(
                "directory key `{key}` holds `{VERSION_FIELD}` = `{value}`, which is not a \
                 version ({error}); refusing to read it as the empty directory"
            ))
        }),
    }
}

/// Redis implementation of [`DocumentStore`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed connection,
/// exactly like [`RedisSessionStore`](crate::RedisSessionStore) and its three
/// siblings.
#[derive(Clone)]
pub struct RedisDocumentStore {
    conn: ConnectionManager,
    scripts: Arc<scripts::Scripts>,
    namespace: KeyNamespace,
}

impl RedisDocumentStore {
    /// Connect under the default namespace (`rh`) and fail fast: a directory
    /// that cannot reach its Redis at startup should stop the process there
    /// rather than on the first admin call.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, DocumentStoreError> {
        Self::connect_namespaced(url, KeyNamespace::default()).await
    }

    /// Connect under an explicit [`KeyNamespace`] — what the composition root
    /// calls once it has read `ROUNDHOUSE_REDIS_NAMESPACE` (R-S3).
    ///
    /// Through `crate::connect_manager` (private, so not a doc-link) for the
    /// reason every other family in this crate goes through it: the outage
    /// latency this crate bounds once rather than per call site.
    pub async fn connect_namespaced(
        url: impl AsRef<str>,
        namespace: KeyNamespace,
    ) -> Result<Self, DocumentStoreError> {
        let conn = crate::connect_manager(url.as_ref())
            .await
            .map_err(backend)?;
        Ok(Self {
            conn,
            scripts: Arc::new(scripts::Scripts::new()),
            namespace,
        })
    }
}

#[async_trait]
impl DocumentStore for RedisDocumentStore {
    async fn load(&self) -> Result<VersionedDocument, DocumentStoreError> {
        let key = records_key(&self.namespace);
        // One HMGET, so the version and the bytes it describes are read in one
        // round trip and cannot be torn across two. `Vec<u8>` and not `String`
        // for the document: it is opaque by contract, and a UTF-8 decode here
        // would fail on the first document a compressed or binary encoding
        // ever puts through this store.
        let (version, document): (Option<String>, Option<Vec<u8>>) = redis::cmd("HMGET")
            .arg(&key)
            .arg(VERSION_FIELD)
            .arg(DOCUMENT_FIELD)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend_at(&key))?;
        Ok(VersionedDocument {
            version: decode_version(version, &key)?,
            document,
        })
    }

    async fn commit(
        &self,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<u64, DocumentStoreError> {
        self.scripts
            .commit(
                &mut self.conn.clone(),
                &records_key(&self.namespace),
                expected_version,
                document,
            )
            .await
    }

    async fn version(&self) -> Result<u64, DocumentStoreError> {
        let key = records_key(&self.namespace);
        // One HGET, and the whole reason `version()` is a method: the document
        // is the deployment's entire tenancy and can be megabytes, so a node
        // whose TTL has elapsed on a quiet deployment pays one integer read
        // rather than a full load.
        let raw: Option<String> = redis::cmd("HGET")
            .arg(&key)
            .arg(VERSION_FIELD)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend_at(&key))?;
        decode_version(raw, &key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key carries the namespace, the version and its family, and carries
    /// no hash tag.
    ///
    /// A unit test rather than a gated one, for the reason the correlation
    /// family's key test is: key strings are pure formatting, and an
    /// ignore-gated duplicate would take a dependency on infrastructure it
    /// does not use. The tag half is the assertion with teeth — a tag added
    /// here later would pin the deployment's whole tenancy to one Cluster
    /// slot in exchange for atomicity this family already has from being one
    /// key (see the module doc).
    #[test]
    fn the_one_key_carries_the_namespace_the_version_and_no_hash_tag() {
        let key = records_key(&KeyNamespace::default());
        assert_eq!(key, "rh:v1:dir:records");
        assert!(
            !key.contains('{') && !key.contains('}'),
            "this family has no multi-key operation to make single-slot, so a \
             hash tag would pin the deployment's tenancy to one slot and buy \
             nothing: {key}"
        );

        // A different namespace must never build the same key -- the whole of
        // R-S3 for a family whose one key would otherwise be shared by every
        // deployment on one Redis.
        let other = KeyNamespace::new("acme-prod").unwrap();
        assert_eq!(records_key(&other), "acme-prod:v1:dir:records");
    }

    /// An absent version field is the empty directory; a field this store did
    /// not write is not.
    ///
    /// The second half is the one that matters: read as zero, a foreign
    /// `version` would make the very next `commit(0, ..)` succeed and
    /// overwrite it.
    #[test]
    fn an_absent_version_is_zero_and_a_foreign_one_is_refused() {
        assert_eq!(decode_version(None, "rh:v1:dir:records").unwrap(), 0);
        assert_eq!(
            decode_version(Some("7".into()), "rh:v1:dir:records").unwrap(),
            7
        );

        let error = decode_version(Some("v2".into()), "rh:v1:dir:records")
            .expect_err("a version field that is not a number is not a version");
        let text = error.to_string();
        assert!(text.contains("rh:v1:dir:records"), "{text}");
        assert!(
            text.contains("refusing to read it as the empty directory"),
            "{text}"
        );
    }
}
