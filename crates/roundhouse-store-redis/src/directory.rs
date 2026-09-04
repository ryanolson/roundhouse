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
//! | `rh:v1:dir:records` | hash | field `version` (decimal), field `lineage` (this run of the counter) and field `document` (opaque bytes) |
//!
//! # R-D6 — one key, three fields, and no hash tag
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
//! **Fields rather than keys**, and that is what makes the compare-and-set
//! expressible at all: the version, the lineage and the document they describe
//! are written by one `HSET`, so no reader can ever see a version that does not
//! match the bytes beside it. Separate keys would need a multi-key script *and*
//! a hash tag to be Cluster-safe, to buy exactly the atomicity one hash already
//! has.
//!
//! # The lineage is why a lost key is not a quiet restart (R-D2″)
//!
//! The counter lives in the key, so a key that is deleted, flushed or restored
//! from an older backup has no memory of the versions it handed out and would
//! begin again at 1 — a version some node is *already serving*. That node's
//! regression check compares numbers, and a number that came back around is
//! not lower than the one it claimed: it would go on serving a plane the
//! deployment has replaced, with nothing anywhere saying so (M16.1 review,
//! F1). So every write carries a `lineage`, minted by the commit script the
//! one time it finds a key with none, and a reader compares the pair. A store
//! that lost its key changes lineage rather than restarting the counter
//! silently, which turns an invisible ABA into a named regression one crate
//! up.
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
//! Only `commit` is a script. `load` and `version` are each one `HMGET`,
//! because neither has a condition in it. `commit` has exactly one —
//! "the store is still at the version this writer read" — and it is the one
//! condition that must not be evaluated against a value another node is
//! replacing: two nodes that each read version *n* and each wrote would leave
//! one of the two changes silently gone, which for this family means a
//! revocation overwritten by a concurrent rename. A Redis script executes with
//! nothing in between, so the check and the write are one indivisible step.
//!
//! # A field this store did not write fails loudly
//!
//! **One grammar, applied identically on both paths.** The key this store
//! wrote either does not exist at all — the empty directory, version zero — or
//! it holds `version` (one to fifteen decimal digits, never zero: the counter
//! starts at one) and `lineage` (the shape [`decode_lineage`] describes)
//! *together*. Every other shape is a foreign writer, an `HDEL`, or a
//! half-finished restore, and fails both the read and the write rather than
//! being read as zero — the same rule the correlation maps apply to an
//! untagged binding, and for the sharper version of the same reason: reading
//! it as zero would make the next `commit(0, ..)` succeed and overwrite
//! whatever is there. A store that cannot say what version it is at must
//! refuse, and the boot above turns that refusal into a stopped process with a
//! reason (R-D8).
//!
//! The two paths enforcing the *same* grammar is the point rather than a
//! tidiness (M16.1 review, F3 and F4). Lua's `tonumber` takes hex, exponent
//! and whitespace forms that `str::parse::<u64>` refuses, and an absent
//! `version` beside a present `document` reads as zero to both — so before
//! this rung a key could be called corrupt by `load` and quietly taken over by
//! `commit`, or the other way round. A refusal that only one half of the store
//! makes is not a refusal.
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

use roundhouse_core::control::directory::{
    DocumentStore, DocumentStoreError, DocumentVersion, VersionedDocument,
};

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

/// The hash field naming this run of the counter (R-D2″). See
/// [`VERSION_FIELD`] for why it is a constant.
pub(crate) const LINEAGE_FIELD: &str = "lineage";

/// The longest lineage this store will read back.
///
/// What it writes is a hyphenated UUID (36 characters); the ceiling is loose
/// enough that a later shape does not need a migration and tight enough that
/// an error message quoting a refused value stays readable. Enforced here and
/// in the commit script, because a grammar only one path applies is the defect
/// this constant exists to close (M16.1 review, F3).
const LINEAGE_MAX_LEN: usize = 64;

/// The longest stored `version` this store will read back.
///
/// Fifteen digits keeps every value inside the range Lua's doubles represent
/// exactly (2^53), which is what lets the commit script do arithmetic on the
/// counter at all — and a counter of admin writes that reached sixteen digits
/// would be a foreign value long before it was a real one. Same ceiling in the
/// script, for the same reason as [`LINEAGE_MAX_LEN`].
const VERSION_MAX_DIGITS: usize = 15;

/// How long a `dir`-family command may wait for a reply, in place of the
/// crate's shared 300ms (M16.1 review, F6).
///
/// **Sized to carry this family's own ceiling with margin, not the crate's
/// ceiling-check budget.** `commit` wraps the entire document — up to
/// [`DIRECTORY_DOCUMENT_CEILING_BYTES`](https://docs.rs/roundhouse-server) —
/// in one Lua argument, and `load` reads it back in one `HMGET`; the pinned
/// redis client wraps enqueue-to-parsed-reply in one `Runtime::timeout`, so
/// the response timeout has to cover the whole transfer, not just a round
/// trip. Measured on this box against a real Redis 7.x over loopback (the
/// most favourable network this budget will ever see — every hop in a real
/// deployment only shrinks how much data a fixed timeout can move): the
/// crate's shared 300ms times a document out around 50 MiB, and 5 seconds
/// carries 32 MiB with room to spare while 64 MiB still times out at the
/// crate's 300ms — comfortably past the 8 MiB ceiling this family actually
/// enforces (`DIRECTORY_DOCUMENT_CEILING_BYTES` in `roundhouse-server`,
/// `control_config::directory::document`), which is the number that matters:
/// nothing this family is contractually asked to store should ever approach
/// this budget.
pub(crate) const DIRECTORY_RESPONSE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

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
/// A present-but-unparseable field is not zero and must never be read as one:
/// zero is the version a first commit expects, so a store that answered zero
/// for a field holding something else would admit that commit and overwrite
/// whatever a foreign writer had put there — turning "I do not understand this
/// key" into a silent clobber.
///
/// The grammar is deliberately narrower than `u64` (plain digits, at most
/// [`VERSION_MAX_DIGITS`] of them, never zero) and is **the same grammar the
/// commit script applies**: a decoder stricter or looser than the writer beside
/// it gives two different answers about one key, which is the whole of the
/// review's F3.
fn decode_version(value: &str, key: &str) -> Result<u64, DocumentStoreError> {
    let refuse = |why: &str| {
        Err(DocumentStoreError::Unavailable(format!(
            "directory key `{key}` holds `{VERSION_FIELD}` = `{value}`, which is not a version \
             ({why}); refusing to read it as the empty directory"
        )))
    };
    if value.is_empty()
        || value.len() > VERSION_MAX_DIGITS
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return refuse("this store writes one to fifteen decimal digits");
    }
    match value.parse::<u64>() {
        Ok(0) => refuse("this counter starts at one"),
        Ok(version) => Ok(version),
        Err(error) => refuse(&error.to_string()),
    }
}

/// Read a stored `lineage` field, refusing anything this store did not write.
///
/// The shape is what this store writes — a hyphenated UUID — checked as
/// "hex digits and hyphens, at most [`LINEAGE_MAX_LEN`] of them" rather than
/// parsed as a UUID: the lineage is opaque to every reader above (it is only
/// ever compared for equality), so what is worth enforcing is that it is a
/// value *this* store produced and not a foreign writer's marker, which the
/// character set answers as well as a parse would and without pinning the
/// format for a future one.
fn decode_lineage(value: &str, key: &str) -> Result<String, DocumentStoreError> {
    if value.is_empty()
        || value.len() > LINEAGE_MAX_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() || byte == b'-')
    {
        return Err(DocumentStoreError::Unavailable(format!(
            "directory key `{key}` holds `{LINEAGE_FIELD}` = `{value}`, which is not a lineage \
             this store wrote; refusing to read it as the empty directory"
        )));
    }
    Ok(value.to_string())
}

/// The pair, decoded together — because the shapes this store writes are the
/// two together or neither.
///
/// A key holding one without the other is a foreign writer, an `HDEL`, or a
/// restore that landed half a hash, and the one thing it must not be read as
/// is the empty store: that is the version a first commit expects, so reading
/// it as zero would admit the very next `commit(0, ..)` over a key whose true
/// version this store never observed (M16.1 review, F4).
fn decode_identity(
    version: Option<String>,
    lineage: Option<String>,
    key: &str,
) -> Result<DocumentVersion, DocumentStoreError> {
    match (version, lineage) {
        (None, None) => Ok(DocumentVersion {
            lineage: String::new(),
            version: 0,
        }),
        (Some(version), Some(lineage)) => Ok(DocumentVersion {
            version: decode_version(&version, key)?,
            lineage: decode_lineage(&lineage, key)?,
        }),
        (present, _) => Err(DocumentStoreError::Unavailable(format!(
            "directory key `{key}` holds `{}` without `{}`; refusing to read a half-written \
             key as the empty directory",
            if present.is_some() {
                VERSION_FIELD
            } else {
                LINEAGE_FIELD
            },
            if present.is_some() {
                LINEAGE_FIELD
            } else {
                VERSION_FIELD
            }
        ))),
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
    /// Through `crate::connect_manager_at` (private, so not a doc-link) at
    /// [`DIRECTORY_RESPONSE_TIMEOUT`] rather than `crate::connect_manager`'s
    /// shared 300ms (M16.1 review, F6) — every other tuning knob (the
    /// connection timeout, the reconnect backoff, the retry count) is still
    /// the crate's shared one, because this family's own contract only ever
    /// asked for a longer response budget, not a different reconnect posture.
    pub async fn connect_namespaced(
        url: impl AsRef<str>,
        namespace: KeyNamespace,
    ) -> Result<Self, DocumentStoreError> {
        let conn = crate::connect_manager_at(url.as_ref(), DIRECTORY_RESPONSE_TIMEOUT)
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
        let (version, lineage, document): (Option<String>, Option<String>, Option<Vec<u8>>) =
            redis::cmd("HMGET")
                .arg(&key)
                .arg(VERSION_FIELD)
                .arg(LINEAGE_FIELD)
                .arg(DOCUMENT_FIELD)
                .query_async(&mut self.conn.clone())
                .await
                .map_err(backend_at(&key))?;
        // The identity is decoded before the document is looked at, so a key
        // holding a document and nothing else -- a foreign writer, an `HDEL`,
        // a half-finished restore -- refuses here instead of arriving above as
        // `Some(bytes)` at version zero, which is a shape the contract does
        // not have a meaning for (M16.1 review, F4).
        let identity = decode_identity(version, lineage, &key)?;
        if identity.version == 0 && document.is_some() {
            return Err(DocumentStoreError::Unavailable(format!(
                "directory key `{key}` holds `{DOCUMENT_FIELD}` with no `{VERSION_FIELD}`; \
                 refusing to read it as the empty directory"
            )));
        }
        Ok(VersionedDocument {
            version: identity.version,
            lineage: identity.lineage,
            document,
        })
    }

    async fn commit(
        &self,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<DocumentVersion, DocumentStoreError> {
        self.scripts
            .commit(
                &mut self.conn.clone(),
                &records_key(&self.namespace),
                expected_version,
                document,
                // Minted per call and spent only if the key holds no lineage
                // of its own: a Lua script may not produce randomness it
                // stores, so a candidate that is almost always discarded is
                // what keeps "is this key already ours" inside the one atomic
                // step (R-D2″).
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
    }

    async fn version(&self) -> Result<DocumentVersion, DocumentStoreError> {
        let key = records_key(&self.namespace);
        // One HMGET of two small fields, and the whole reason `version()` is a
        // method: the document is the deployment's entire tenancy and can be
        // megabytes, so a node whose TTL has elapsed on a quiet deployment
        // pays two short reads rather than a full load. The lineage rides with
        // the number rather than being fetched when it looks suspicious --
        // there is no "looks suspicious": a restarted counter looks exactly
        // like a quiet deployment until the pair is compared.
        let (version, lineage): (Option<String>, Option<String>) = redis::cmd("HMGET")
            .arg(&key)
            .arg(VERSION_FIELD)
            .arg(LINEAGE_FIELD)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend_at(&key))?;
        decode_identity(version, lineage, &key)
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
        let key = "rh:v1:dir:records";
        assert_eq!(decode_identity(None, None, key).unwrap().version, 0);
        assert_eq!(decode_version("7", key).unwrap(), 7);

        let error = decode_version("v2", key)
            .expect_err("a version field that is not a number is not a version");
        let text = error.to_string();
        assert!(text.contains(key), "{text}");
        assert!(
            text.contains("refusing to read it as the empty directory"),
            "{text}"
        );
    }

    /// The numeric grammar is exactly the commit script's, and the shapes
    /// between the two are what the M16.1 review's F3 was about.
    ///
    /// Lua's `tonumber` accepts every value in the first list; `str::parse`
    /// accepts none of them. Before this rung that difference was live: the
    /// read path called such a key corrupt while the write path read it as a
    /// number of its own choosing and overwrote the document beside it. The
    /// unit test is here rather than only against a real Redis because this is
    /// the half of the pair that costs nothing to run — the Lua half is pinned
    /// in `tests/directory_contract.rs`, and a change to one that forgets the
    /// other now fails one of the two.
    #[test]
    fn the_numeric_grammar_is_the_commit_scripts() {
        let key = "rh:v1:dir:records";
        for lua_would_take in [
            "0x0",
            "0x10",
            "1e3",
            " 7 ",
            " 0",
            "7.0",
            "0.0",
            "-0",
            "0e0",
            "0",
            // Sixteen digits: inside `u64`, outside the range Lua's doubles
            // count in exactly, so the script refuses it and so does this.
            "1234567890123456",
        ] {
            assert!(
                decode_version(lua_would_take, key).is_err(),
                "`{lua_would_take}` is not a version this store writes, and a read path that \
                 took it would disagree with the commit script about the same key"
            );
        }
        assert_eq!(
            decode_version("999999999999999", key).unwrap(),
            999_999_999_999_999
        );
    }

    /// A lineage is the shape this store writes, and half a key is not a key.
    ///
    /// The second half is the M16.1 review's F4: a hash holding one field of
    /// the pair is a foreign writer or a partial restore, and the one thing it
    /// must not decode to is the empty store's zero -- the version the next
    /// commit expects.
    #[test]
    fn a_lineage_is_this_stores_shape_and_half_a_key_is_refused() {
        let key = "rh:v1:dir:records";
        let minted = uuid::Uuid::new_v4().to_string();
        assert_eq!(decode_lineage(&minted, key).unwrap(), minted);

        for foreign in ["", "not a lineage", "ABCDEF", &"a".repeat(65)] {
            assert!(
                decode_lineage(foreign, key).is_err(),
                "`{foreign}` is not a value this store wrote"
            );
        }

        for half in [(Some("7".to_string()), None), (None, Some(minted.clone()))] {
            let error = decode_identity(half.0, half.1, key)
                .expect_err("half a key is not the empty directory");
            assert!(error.to_string().contains("half-written"), "{error}");
        }
    }
}
