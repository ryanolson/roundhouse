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

/// M16.1 review, F1 (R-D2″): a key that was deleted starts a **new lineage**
/// rather than silently restarting the counter.
///
/// `COMMIT` reads "no version field" as "empty store, count from 0" — true for
/// a namespace that has never been written, and equally true the instant an
/// operator (or a failover, or a restore) deletes the key out from under a
/// live deployment. The counter cannot help itself here: it lives in the key
/// that just vanished, so the next commit really does hand out version 1
/// again, a number this store already returned once.
///
/// What the fix changes is not the number but whether anyone can *tell*.
/// Before the lineage, a node serving version 1 asked `version()`, was told
/// `1`, and concluded nothing had happened — the exact ABA the reader's
/// regression check compares `<` for and therefore cannot see. Now the pair
/// differs, and the reader one crate up turns that into a named regression
/// (`a_counter_that_restarted_at_the_served_version_is_not_read_as_unchanged`,
/// in `roundhouse-server`).
///
/// Deterministic, with no concurrency and so no clock or signal: one commit,
/// one raw `DEL` standing in for the operator/failover/restore case, one more
/// commit.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_deleted_key_starts_a_new_lineage_rather_than_restarting_silently() {
    let namespace = fresh_namespace();
    let store = connect_to(namespace.clone()).await;
    let mut raw = raw_from_env().await;

    let first = store.commit(0, b"before".to_vec()).await.unwrap();
    assert_eq!(
        first.version, 1,
        "the deployment's very first commit is version 1"
    );

    // Stands in for an operator DEL, a FLUSHDB, or a restore from an older
    // backup: the key this family owns is gone, exactly as if this namespace
    // had never been written.
    let key = directory_records_key(&namespace);
    let deleted: i64 = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(
        deleted, 1,
        "the key must have existed to prove anything about deleting it"
    );

    // The empty store it is now: no version, and no lineage to claim.
    let emptied = store.version().await.unwrap();
    assert_eq!(emptied.version, 0);
    assert!(
        emptied.lineage.is_empty(),
        "a key that does not exist has no lineage to answer with, which is \
         what makes the first commit below able to mint one"
    );

    let second = store.commit(0, b"after".to_vec()).await.unwrap();
    assert_ne!(
        second.lineage, first.lineage,
        "a store that lost its key must say so: the counter is allowed to \
         restart -- it has no memory to restart from -- but a version already \
         handed out once (here: {first:?}) must never come back looking like \
         the same document's next state, which is exactly what a reader \
         comparing `version == claimed_version` cannot tell from a quiet \
         deployment"
    );
    assert_eq!(
        store.version().await.unwrap(),
        second,
        "and the cheap read answers the new lineage too -- it is the call the \
         reader's refresh actually makes"
    );
}

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
    let committed = first.commit(0, written.clone()).await.unwrap();
    let loaded = second.load().await.unwrap();
    assert_eq!(loaded.version, committed.version);
    assert_eq!(
        loaded.lineage, committed.lineage,
        "and the two nodes agree about which run of the counter that version \
         belongs to -- a lineage that were per handle rather than per key \
         would make every neighbour's write look like a flushed store (R-D2″)"
    );
    assert_eq!(
        loaded.document.as_deref(),
        Some(written.as_slice()),
        "a directory written on one node must be the directory the next node \
         compiles its plane from -- this is the whole of the M8 unlock \
         condition"
    );

    // (c) The cheap read agrees across handles too, which is what makes the
    // other node's refresh able to skip the load.
    assert_eq!(second.version().await.unwrap(), committed);

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
            }) if found == committed.version
        ),
        "the compare-and-set is what stops one node's revocation being \
         overwritten by another node's concurrent rename: {stale:?}"
    );

    // CONTROL: the second handle is not refusing everything. Against the
    // version the store is genuinely at, its write lands and the first handle
    // sees it -- so (d) is about the version comparison rather than about a
    // second handle that cannot write at all.
    let next = second
        .commit(committed.version, b"from-second".to_vec())
        .await
        .unwrap();
    assert_eq!(
        first.load().await.unwrap().document.as_deref(),
        Some(b"from-second".as_slice())
    );
    assert!(next.version > committed.version);
    assert_eq!(
        next.lineage, committed.lineage,
        "an ordinary write from another node is the same key, still there"
    );
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
        vec![
            "document".to_string(),
            "lineage".to_string(),
            "version".to_string()
        ],
        "exactly the three fields the module doc's key table names"
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

    let stored_lineage: String = redis::cmd("HGET")
        .arg(&key)
        .arg("lineage")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        uuid::Uuid::parse_str(&stored_lineage).is_ok(),
        "the lineage this store mints is what its own decoder accepts, and a \
         value an operator can see is the same one two nodes compare: \
         {stored_lineage}"
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

/// M16.1 review, F3: the commit script's numeric grammar is the Rust
/// decoder's, so a foreign value is refused by **both** halves of this store or
/// by neither.
///
/// Lua's `tonumber` accepts hex (`0x10`), exponent notation (`1e3`), surrounding
/// whitespace and decimal-point forms; `str::parse::<u64>` accepts none of
/// them. Before this rung that difference was live and pointed the wrong way:
/// `load`/`version` called such a key corrupt and refused, while `commit` read
/// it as a number of its own choosing (`0x0` as zero, `0x10` as sixteen) and
/// overwrote the foreign document beside it — the exact clobber this store's
/// module doc says cannot happen, "on the read path and on the write path".
///
/// Each case writes a *complete* foreign key — a valid lineage beside the
/// suspect version — so what is under test is the numeric grammar itself and
/// not the missing-field rule the case below covers. The expected version
/// handed to `commit` is the number Lua's `tonumber` would have computed, so a
/// script that still used the loose grammar would find its comparison
/// satisfied and admit the write.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_numeric_looking_foreign_version_is_refused_on_commit_too() {
    let mut raw = raw_from_env().await;

    // (stored version, the version `tonumber` would read it as -- what a
    // writer fooled by the loose grammar would pass as `expected`).
    for (foreign, as_lua_reads_it) in [
        ("0x0", 0),
        ("0x10", 16),
        ("1e3", 1000),
        (" 0", 0),
        (" 7 ", 7),
        ("7.0", 7),
        ("0.0", 0),
        ("0e0", 0),
        ("-0", 0),
        // Not a `tonumber` case: a plain zero, which the empty store's own
        // `commit(0, ..)` expects. This store's counter starts at one, so a
        // stored zero is a foreign writer's value and reading it as the empty
        // directory would hand this key away on the next write.
        ("0", 0),
    ] {
        let namespace = fresh_namespace();
        let store = connect_to(namespace.clone()).await;
        let key = directory_records_key(&namespace);
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("version")
            .arg(foreign)
            .arg("lineage")
            .arg(uuid::Uuid::new_v4().to_string())
            .arg("document")
            .arg("someone else's")
            .query_async(&mut raw)
            .await
            .unwrap();

        // The read path refuses -- the control, which passed before the fix
        // too, and is what makes the write path's answer a *disagreement*
        // rather than a second opinion.
        assert!(
            matches!(store.load().await, Err(DocumentStoreError::Unavailable(_))),
            "the read path must refuse `{foreign}`"
        );
        assert!(matches!(
            store.version().await,
            Err(DocumentStoreError::Unavailable(_))
        ));

        let committed = store.commit(as_lua_reads_it, b"mine now".to_vec()).await;
        assert!(
            matches!(committed, Err(DocumentStoreError::Unavailable(_))),
            "the write path must refuse `{foreign}` exactly as the read path \
             does, but commit({as_lua_reads_it}, ..) returned {committed:?}"
        );

        let still: String = redis::cmd("HGET")
            .arg(&key)
            .arg("document")
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(
            still, "someone else's",
            "refusing is only worth anything if the refusal did not write first"
        );
    }

    // The lineage field has a grammar of its own, and for the same reason:
    // a value this store did not write is a key this store does not own.
    let namespace = fresh_namespace();
    let store = connect_to(namespace.clone()).await;
    let key = directory_records_key(&namespace);
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg("version")
        .arg("1")
        .arg("lineage")
        .arg("not a lineage this store minted")
        .arg("document")
        .arg("someone else's")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(matches!(
        store.load().await,
        Err(DocumentStoreError::Unavailable(_))
    ));
    assert!(
        matches!(
            store.commit(1, b"mine now".to_vec()).await,
            Err(DocumentStoreError::Unavailable(_))
        ),
        "a foreign lineage is refused on the write path too -- otherwise this \
         store would adopt a key it never wrote and carry that value forward \
         as its own identity"
    );

    // CONTROL: the same shapes, written by this store, still work. Without
    // this, a decoder that refused everything would pass every assertion above.
    let store = connect_in_a_fresh_namespace().await;
    let first = store.commit(0, b"ours".to_vec()).await.unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(
        store
            .commit(1, b"ours again".to_vec())
            .await
            .unwrap()
            .version,
        2
    );
}

/// M16.1 review, F4: version 0 is defined as "no document" -- the core
/// doc (`roundhouse-core/src/control/directory.rs`, `VersionedDocument`)
/// says `document: None` at `version: 0` is the empty store, and says so by
/// deliberately *not* collapsing the two facts into one, because the memory
/// store's `(Option<Vec<u8>>, u64)` only ever moves both fields together and
/// starts at `(None, 0)` -- it can never produce `Some(bytes)` at version 0.
///
/// A Redis hash can: `HGET version` on a key holding only a `document` field
/// (a foreign writer, an `HDEL version`, a partial restore) returns `false`,
/// which both the read path (`decode_version`, directory.rs:150) and the
/// commit script's `held == false` branch (`directory/scripts.rs:55-56`) read
/// as the *absent-key* zero -- the same zero an empty store answers. Nothing
/// distinguishes "this key has never been written" from "this key has a
/// document but its version field is gone", so `load()` hands back
/// `VersionedDocument { version: 0, document: Some(bytes) }`, and the very
/// next `commit(0, ..)` is admitted as though it were the deployment's first
/// write, overwriting a key whose true version this store never observed.
///
/// Deterministic, no clock or concurrency needed: one raw `HSET` with a
/// `document` field and no `version` field, then `load()` and `commit(0,
/// ..)` against that same key.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_document_with_no_version_field_is_refused_not_read_as_version_zero() {
    let mut raw = raw_from_env().await;
    let namespace = fresh_namespace();
    let store = connect_to(namespace.clone()).await;
    let key = directory_records_key(&namespace);

    // Stands in for a foreign writer, an `HDEL version`, or a partial
    // restore: the hash holds a document but has no version field of its
    // own -- a shape `MemoryDocumentStore` can never reach, because its two
    // fields move together starting from `(None, 0)`.
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg("document")
        .arg("foreign document, no version field")
        .query_async(&mut raw)
        .await
        .unwrap();

    let loaded = store.load().await;
    assert!(
        matches!(loaded, Err(DocumentStoreError::Unavailable(_))),
        "a hash holding a document with no version field must be refused as \
         unavailable -- the mirror of `None` at a nonzero version, which is \
         already refused -- but load() returned {loaded:?}"
    );

    let committed = store.commit(0, b"mine now".to_vec()).await;
    assert!(
        matches!(committed, Err(DocumentStoreError::Unavailable(_))),
        "commit(0, ..) must not be admitted over a key whose true version \
         was never observed, but it returned {committed:?}"
    );

    // The foreign document must still be exactly where it was: refusing is
    // only worth anything if the refusal did not write first.
    let still: Vec<u8> = redis::cmd("HGET")
        .arg(&key)
        .arg("document")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(
        still,
        b"foreign document, no version field".to_vec(),
        "the foreign document must be untouched"
    );
}

/// The cheap read really is cheap: `version()` is one round trip and never
/// touches the document.
///
/// **The cost R-D6 budgets for, counted rather than asserted.** The refresh
/// above asks `version()` on every TTL of a quiet deployment and loads only
/// when the answer moves — an arrangement that is worth nothing if `version()`
/// itself fetches a document that can be megabytes. A test that counted the
/// calls it made would be counting its own arithmetic; what is at risk is what
/// the *client library* sends, so this counts Redis's own `commandstats`.
///
/// One `HMGET` and not one `HGET` since R-D2″: the lineage rides back with the
/// number, because a restarted counter is invisible until the pair is compared
/// (M16.1 review, F1). That makes the command count alone too weak a pin — a
/// `load` is an `HMGET` too — so the bytes Redis actually sent are measured
/// beside it: two short fields against a document three orders of magnitude
/// bigger, so a `version()` that reached for the document could not hide in the
/// noise of anything else on this server.
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
    store.commit(0, vec![b'x'; DOCUMENT]).await.unwrap();

    const ATTEMPTS: usize = 25;
    const DOCUMENT: usize = 512 * 1024;
    let mut round_trips = u64::MAX;
    let mut bytes_out = u64::MAX;
    for _ in 0..ATTEMPTS {
        let before = (
            calls(&mut raw, "hget").await + calls(&mut raw, "hmget").await,
            net_output_bytes(&mut raw).await,
        );
        assert_eq!(store.version().await.unwrap().version, 1);
        let after = (
            calls(&mut raw, "hget").await + calls(&mut raw, "hmget").await,
            net_output_bytes(&mut raw).await,
        );
        round_trips = round_trips.min(after.0 - before.0);
        bytes_out = bytes_out.min(after.1 - before.1);
    }

    assert_eq!(
        round_trips, 1,
        "a version read must be exactly one hash read: a read that fanned out \
         would make the quiet-node refresh cost more than R-D6 budgets"
    );
    assert!(
        bytes_out < (DOCUMENT / 8) as u64,
        "and it must not reach for the document, which is the entire point of \
         `version()` being a method of its own: the smallest reply this server \
         sent across {ATTEMPTS} attempts was {bytes_out} bytes against a \
         {DOCUMENT}-byte document (the measurement also carries the `INFO` \
         replies this loop itself asks for, so it is an upper bound)"
    );
}

/// How many of one command this Redis has served in its life.
///
/// Absent from `commandstats` until the first one, which is why a missing
/// counter reads as zero rather than as a failure.
/// Total bytes this server has written to clients, from `INFO stats`.
///
/// Server-wide and monotone, like the `commandstats` counters beside it, so the
/// *minimum* delta across attempts is a true upper bound on what one call cost:
/// anything else talking to this Redis can only inflate an attempt.
async fn net_output_bytes(raw: &mut redis::aio::MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("stats")
        .query_async(raw)
        .await
        .expect("INFO stats is available on every Redis this suite supports");
    info.lines()
        .find_map(|line| line.strip_prefix("total_net_output_bytes:"))
        .and_then(|value| value.trim().parse().ok())
        .expect("INFO stats always reports total_net_output_bytes")
}

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

/// F6 (M16.1 review): `connect_manager` (`lib.rs`) used to give every family
/// in this crate the same `RESPONSE_TIMEOUT` -- 300ms, and the doc beside the
/// constant named exactly one reason for that number: "this manager sits
/// under a ceiling check with its own two-second budget". The directory
/// family answers to a different contract. `commit` wraps a Lua script whose
/// argument and whose `HSET` value are the *entire document* -- possibly
/// megabytes, per this family's own module doc -- and the pinned redis
/// client (`send_recv`, `multiplexed_connection.rs`) wraps
/// enqueue-to-parsed-reply in one `Runtime::timeout(response_timeout, ..)`.
/// Once that future is `Elapsed`, the client turns it into
/// `io::Error::from(ErrorKind::TimedOut)`, which this crate's `backend_at`
/// carries into `DocumentStoreError::Unavailable` -- indistinguishable, to a
/// caller, from a Redis that is actually down, and (per the connection
/// manager's own retry policy) a trigger for a full reconnect rather than
/// "the document is large, wait longer."
///
/// The fix has two halves, and this test is the half a real Redis can prove:
/// `RedisDocumentStore` now connects at `DIRECTORY_RESPONSE_TIMEOUT` (5s)
/// rather than the crate's shared 300ms, sized so a document at this
/// family's own ceiling — `DIRECTORY_DOCUMENT_CEILING_BYTES`, 8 MiB, enforced
/// one crate up in `roundhouse-server`'s typed adapter before any write ever
/// reaches this store — commits and loads with room to spare. The other half
/// (a document over that ceiling is refused before it reaches this crate at
/// all) is proved where the ceiling is enforced,
/// `a_document_at_the_ceiling_commits_and_one_byte_over_is_refused_before_any_wire`
/// in `roundhouse-server`'s `control_config::directory::document::tests` —
/// this crate has no adapter of its own to refuse anything with, only the
/// wire this test exercises. 8 MiB stands in here for "at the ceiling" rather
/// than being computed against the other crate's constant, since this crate
/// does not depend on it; the control below shows the size itself was never
/// the problem, only the budget it used to be carried over.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_document_at_the_directory_ceiling_does_not_time_out_the_response_budget() {
    // Mirrors `DIRECTORY_DOCUMENT_CEILING_BYTES` in
    // `roundhouse-server::control_config::directory::document` (8 MiB) --
    // restated rather than imported, since this crate does not depend on
    // that one. This box's own measured crossover against the crate's
    // *shared* 300ms budget (the docs beside `RESPONSE_TIMEOUT` and
    // `DIRECTORY_RESPONSE_TIMEOUT` carry the numbers) puts the failure point
    // around 50 MiB, so 8 MiB was never close to that line -- proving this
    // size clears `DIRECTORY_RESPONSE_TIMEOUT` with room to spare, not merely
    // clearing the old one by luck.
    const SIZE: usize = 8 * 1024 * 1024;
    let document = vec![b'd'; SIZE];

    // CONTROL: the size alone is not what fails. A store with no wire at all
    // commits and loads the identical bytes with no trouble, so a failure
    // below would be about the response budget, not about 8 MiB being an
    // unreasonable amount of data to hold in memory or pass around.
    let memory = roundhouse_core::control::MemoryDocumentStore::new();
    let memory_version = memory.commit(0, document.clone()).await.unwrap().version;
    assert_eq!(
        memory
            .load()
            .await
            .unwrap()
            .document
            .as_deref()
            .map(|bytes| bytes.len()),
        Some(SIZE),
        "control: a store with no wire and no timeout must hold this document with no trouble"
    );

    let store = connect_in_a_fresh_namespace().await;
    let committed = store.commit(0, document.clone()).await.expect(
        "a document at this family's own ceiling must commit under DIRECTORY_RESPONSE_TIMEOUT",
    );
    assert_eq!(
        committed.version, memory_version,
        "the write landed, but at a different version than the control store's identical \
         commit -- both start from an empty store, so they must agree"
    );
    let loaded = store
        .load()
        .await
        .expect("the same document must load back under the same timeout");
    assert_eq!(
        loaded.document.as_deref().map(<[u8]>::len),
        Some(SIZE),
        "the document must load back whole, not truncated by a reply the timeout cut short"
    );
}
