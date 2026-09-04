// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M16.1: the full [`DocumentStore`] contract against a real Redis, plus the
//! claims only a shared backend can make.
//!
//! The macro invocation is the milestone's headline, in the same idiom as
//! `correlation_contract.rs` and `spend_contract.rs`: the *same* assertions
//! that judge `MemoryDocumentStore` judge this store, whatever the list grows
//! to — the macro is the list, so a test added there is added here with no
//! wiring step. The key-layout assertions need no live Redis (key strings are
//! pure formatting) and live as unit tests beside the function that builds
//! them.
//!
//! **Isolation is a fresh namespace per test, not a fresh id inside one.**
//! Every other family here mints a fresh `Principal` or session id, because
//! every other family is keyed per tenant. This one has a single key for the
//! whole deployment (R-D6), so there is nothing inside it to make fresh and
//! the isolation moves outward to [`KeyNamespace`] — which is also, for free,
//! a second proof that R-S3's namespace really does partition this family.
//!
//! Below the suite is what only a real, *shared* Redis can show:
//!
//! - **the unlock condition itself** — a document committed through one handle
//!   is read by another, which is the whole reason this store exists: until
//!   this rung a project created on node A did not exist on node B and did not
//!   survive a restart;
//! - **the stored shape**, asserted on the hash Redis actually holds rather
//!   than only on what `load` decodes it to, so the test is about the
//!   mechanism rather than about its shadow;
//! - **a foreign key refused rather than clobbered**, in both the read path
//!   and the write path — the failure R-D8 turns into a stopped boot;
//! - **the cheap read's cost**, counted on Redis's own `commandstats` so "a
//!   quiet node pays one integer read per TTL" is a measurement rather than a
//!   claim.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

mod common;

use common::raw_from_env;
use roundhouse_core::control::directory::{DocumentStore, DocumentStoreError};
use roundhouse_store_redis::test_support::{directory_records_key, url_from_env};
use roundhouse_store_redis::{KeyNamespace, RedisDocumentStore};

roundhouse_core::document_store_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_in_a_fresh_namespace().await,
);

/// A namespace nothing else in this run shares.
///
/// The equivalent of the other families' `fresh_principal`, one level out.
/// `KeyNamespace::new` refuses `:`, braces and whitespace, and a UUID's
/// hyphenated form contains none of them.
fn fresh_namespace() -> KeyNamespace {
    KeyNamespace::new(format!("dirtest-{}", uuid::Uuid::new_v4()))
        .expect("a hyphenated UUID contains no character the namespace rejects")
}

/// A store over the environment's Redis, under a namespace of its own — the
/// suite's `$make`, evaluated inside every generated test.
async fn connect_in_a_fresh_namespace() -> RedisDocumentStore {
    connect_to(fresh_namespace()).await
}

async fn connect_to(namespace: KeyNamespace) -> RedisDocumentStore {
    RedisDocumentStore::connect_namespaced(url_from_env(), namespace)
        .await
        .expect("Redis named by the env var must be reachable")
}

/// **The unlock condition, and the whole reason this family exists.**
///
/// Two handles are two nodes: separate connections, separate script caches,
/// nothing shared but the Redis and the namespace. Every assertion here was
/// false of the `Mutex`-backed directory this store replaces — a project
/// created on one node did not exist on the next one, and nothing survived a
/// restart.
///
/// The four are one test because they are one property, and splitting them
/// would let three pass while the fourth quietly exercised a different pair of
/// handles.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn what_one_node_commits_is_what_the_other_node_loads() {
    let namespace = fresh_namespace();
    let first = connect_to(namespace.clone()).await;
    let second = connect_to(namespace).await;

    // (a) A namespace nothing has written is the empty directory on both
    // handles, which is what lets a fresh deployment boot with no seeding
    // step.
    assert_eq!(second.load().await.unwrap().version, 0);
    assert_eq!(second.load().await.unwrap().document, None);

    // (b) A document committed on one handle is loaded, exactly, by the other.
    let written = b"{\"schema\":1,\"records\":{}}".to_vec();
    let version = first.commit(0, written.clone()).await.unwrap();
    let loaded = second.load().await.unwrap();
    assert_eq!(loaded.version, version);
    assert_eq!(
        loaded.document.as_deref(),
        Some(written.as_slice()),
        "a directory written on one node must be the directory the next node \
         compiles its plane from -- this is the whole of the M8 unlock \
         condition"
    );

    // (c) The cheap read agrees across handles too, which is what makes the
    // other node's refresh able to skip the load.
    assert_eq!(second.version().await.unwrap(), version);

    // (d) And the second handle's own commit is gated on what the first one
    // did: a write that still thinks the store is empty is refused, naming
    // the version the first handle left.
    let stale = second.commit(0, b"clobber".to_vec()).await;
    assert!(
        matches!(
            stale,
            Err(DocumentStoreError::Concurrent {
                expected: 0,
                found,
            }) if found == version
        ),
        "the compare-and-set is what stops one node's revocation being \
         overwritten by another node's concurrent rename: {stale:?}"
    );

    // CONTROL: the second handle is not refusing everything. Against the
    // version the store is genuinely at, its write lands and the first handle
    // sees it -- so (d) is about the version comparison rather than about a
    // second handle that cannot write at all.
    let next = second
        .commit(version, b"from-second".to_vec())
        .await
        .unwrap();
    assert_eq!(
        first.load().await.unwrap().document.as_deref(),
        Some(b"from-second".as_slice())
    );
    assert!(next > version);
}

/// The document lives in exactly one hash with exactly two fields, and the
/// version beside it is decimal.
///
/// The contract already asserts what `load` decodes to. What this adds is the
/// mechanism R-D6 names: one key, no hash tag, `version` and `document` moved
/// by one `HSET` so no reader can see a version that does not match the bytes
/// beside it. A store that wrote the two into separate keys would pass every
/// contract assertion on a quiet Redis and tear the moment two writes
/// interleaved.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_document_and_its_version_live_in_one_hash_under_one_key() {
    let namespace = fresh_namespace();
    let store = connect_to(namespace.clone()).await;
    let mut raw = raw_from_env().await;

    // Two bytes that are not UTF-8, so a store that round-tripped the
    // document through a String would fail here rather than in production.
    let document = vec![b'{', b'}', 0x00, 0xff];
    store.commit(0, document.clone()).await.unwrap();

    let key = directory_records_key(&namespace);
    assert_eq!(key, format!("{namespace}:v1:dir:records"));

    let kind: String = redis::cmd("TYPE")
        .arg(&key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(
        kind, "hash",
        "R-D6: one hash, not a string and not two keys"
    );

    let mut fields: Vec<String> = redis::cmd("HKEYS")
        .arg(&key)
        .query_async(&mut raw)
        .await
        .unwrap();
    fields.sort();
    assert_eq!(
        fields,
        vec!["document".to_string(), "version".to_string()],
        "exactly the two fields the module doc's key table names"
    );

    let stored_version: String = redis::cmd("HGET")
        .arg(&key)
        .arg("version")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(
        stored_version, "1",
        "the version is decimal in the store, so an operator reading the key \
         by hand sees the same number the API refuses a stale write against"
    );

    let stored_document: Vec<u8> = redis::cmd("HGET")
        .arg(&key)
        .arg("document")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(stored_document, document, "bytes in, the same bytes stored");

    // And the whole family really is that one key: nothing else in this
    // namespace was written. A store that also kept a sidecar key would make
    // "one atomic read" false without any assertion above noticing.
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{namespace}:*"))
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(keys, vec![key]);
}

/// A key this store did not write is refused — on the read path and on the
/// write path — rather than being read as the empty directory.
///
/// **The failure R-D8 turns into a stopped boot.** Read as zero, a foreign
/// `version` would make the very next `commit(0, ..)` succeed, so the
/// deployment would silently take ownership of somebody else's key; and a
/// `load` that answered "empty" would compile a plane containing the file's
/// entries alone, quietly admitting and refusing the wrong callers. Both
/// answer `Unavailable` with the key named, which is what lets the boot say
/// where to look.
///
/// The two shapes are both here because they fail at different layers: a
/// wrong-type key is refused by Redis itself (`WRONGTYPE`, surfaced through
/// the connection error), and a hash whose `version` field is not a number is
/// refused by this crate's own decode — one of which would keep passing if the
/// other were deleted.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_key_this_store_did_not_write_is_refused_rather_than_overwritten() {
    let mut raw = raw_from_env().await;

    // (a) The key exists and is not a hash at all.
    let wrong_type = fresh_namespace();
    let store = connect_to(wrong_type.clone()).await;
    let _: () = redis::cmd("SET")
        .arg(directory_records_key(&wrong_type))
        .arg("not a directory")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        matches!(store.load().await, Err(DocumentStoreError::Unavailable(_))),
        "a directory key holding the wrong Redis type must refuse the read, \
         not read as an empty directory"
    );
    assert!(matches!(
        store.version().await,
        Err(DocumentStoreError::Unavailable(_))
    ));
    assert!(matches!(
        store.commit(0, b"mine now".to_vec()).await,
        Err(DocumentStoreError::Unavailable(_))
    ));

    // (b) The key is a hash, and its version field is something this store
    // did not write. This one gets past Redis and is caught by the decode.
    let foreign = fresh_namespace();
    let store = connect_to(foreign.clone()).await;
    let key = directory_records_key(&foreign);
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg("version")
        .arg("v2")
        .arg("document")
        .arg("something else's")
        .query_async(&mut raw)
        .await
        .unwrap();

    let refused = store.load().await;
    match refused {
        Err(DocumentStoreError::Unavailable(reason)) => {
            assert!(reason.contains(&key), "the reason names the key: {reason}");
        }
        other => panic!("a foreign version field must refuse the load: {other:?}"),
    }

    let refused = store.commit(0, b"mine now".to_vec()).await;
    match refused {
        Err(DocumentStoreError::Unavailable(reason)) => {
            assert!(reason.contains(&key), "{reason}");
        }
        other => panic!(
            "and it must refuse the write rather than treating the key as \
             empty and taking it over: {other:?}"
        ),
    }

    // The foreign value is still exactly where it was: refusing is only worth
    // anything if the refusal did not write first.
    let still: String = redis::cmd("HGET")
        .arg(&key)
        .arg("version")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(still, "v2");
}

/// The cheap read really is cheap: `version()` is one `HGET` and never
/// touches the document.
///
/// **The cost R-D6 budgets for, counted rather than asserted.** The refresh
/// above asks `version()` on every TTL of a quiet deployment and loads only
/// when the answer moves — an arrangement that is worth nothing if `version()`
/// itself fetches a document that can be megabytes. A test that counted the
/// calls it made would be counting its own arithmetic; what is at risk is what
/// the *client library* sends, so this counts Redis's own `commandstats`.
///
/// The counter is server-wide, so the measurement is the *minimum* delta over
/// several attempts — anything else on this Redis can only inflate an attempt,
/// never deflate one, so the minimum is a true upper bound. Deliberately not
/// `CONFIG RESETSTAT`, for `correlation_contract.rs`'s reason: two binaries
/// resetting one server's stats would each zero the other's window, and a
/// count of zero passes nothing. This is this binary's one commandstats
/// measuring loop.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn one_version_read_is_one_hget_and_never_fetches_the_document() {
    let store = connect_in_a_fresh_namespace().await;
    let mut raw = raw_from_env().await;
    // A document big enough that fetching it would be visibly the wrong thing
    // to do on every TTL of a quiet node.
    store.commit(0, vec![b'x'; 512 * 1024]).await.unwrap();

    const ATTEMPTS: usize = 25;
    let mut hgets = u64::MAX;
    let mut hmgets = u64::MAX;
    for _ in 0..ATTEMPTS {
        let before = (
            calls(&mut raw, "hget").await,
            calls(&mut raw, "hmget").await,
        );
        assert_eq!(store.version().await.unwrap(), 1);
        let after = (
            calls(&mut raw, "hget").await,
            calls(&mut raw, "hmget").await,
        );
        hgets = hgets.min(after.0 - before.0);
        hmgets = hmgets.min(after.1 - before.1);
    }

    assert_eq!(
        hgets, 1,
        "a version read must be exactly one HGET: a read that fanned out would \
         make the quiet-node refresh cost more than the integer R-D6 budgets"
    );
    assert_eq!(
        hmgets, 0,
        "and it must not reach for the document, which is the entire point of \
         `version()` being a method of its own"
    );
}

/// How many of one command this Redis has served in its life.
///
/// Absent from `commandstats` until the first one, which is why a missing
/// counter reads as zero rather than as a failure.
async fn calls(raw: &mut redis::aio::MultiplexedConnection, command: &str) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(raw)
        .await
        .expect("the test Redis must answer INFO commandstats");
    let prefix = format!("cmdstat_{command}:calls=");
    info.lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .and_then(|tail| tail.split(',').next())
        .and_then(|calls| calls.parse().ok())
        .unwrap_or(0)
}
