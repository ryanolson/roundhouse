// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The [`DocumentStore`] contract as executable assertions.
//!
//! Every guarantee the trait documents lives here as a test any backend must
//! pass unchanged, exactly as [`correlation::contract`](super::super::correlation::contract)
//! and the two ledgers' contracts do one seam over.
//! **[`MemoryDocumentStore`](super::MemoryDocumentStore) is the
//! specification**: where a backend's own representation cannot reproduce an
//! assertion here, the backend is wrong.
//!
//! It matters more for this family than for any of the other four, and the
//! reason is arithmetic rather than taste. The ledgers and the correlation
//! maps are keyed per principal, so a backend whose compare-and-set was subtly
//! wrong would corrupt one tenant's row. This family has **one key for the
//! whole deployment** — the document *is* the tenancy — so the same mistake
//! loses every project, user, membership and key the admin plane ever created.
//! One list, run against the Rust store and against the Lua one, is what makes
//! "a stale commit changes nothing" a checked property of both.
//!
//! # Every test starts from an empty store, by requirement on the instantiation
//!
//! Unlike the per-principal families — where every test mints a fresh
//! `Principal` so one shared Redis can host the whole suite — there is nothing
//! *inside* this family to make fresh: one key per namespace is the whole
//! layout (R-D6). So isolation moves outward: the Redis instantiation hands
//! each generated test a handle under a **fresh [`KeyNamespace`]**, which is
//! the same isolation by a different lever, and the memory instantiation gets
//! a fresh store for free because `$make` is evaluated inside each test.
//!
//! [`KeyNamespace`]: https://docs.rs/roundhouse-store-redis

use super::{DocumentStore, DocumentStoreError};

/// The bytes a test commits, distinguishable from any other test's.
///
/// Not JSON and deliberately not valid UTF-8 at one byte: the store is opaque
/// by contract (R-D5), and a backend that round-tripped its document through a
/// `String` would pass a suite written in ASCII and mangle the first document
/// a compressed or binary encoding ever puts through it.
fn document(marker: &str) -> Vec<u8> {
    let mut bytes = format!("document:{marker}:").into_bytes();
    bytes.extend_from_slice(&[0x00, 0xff, 0xfe, 0x80]);
    bytes
}

/// **The claim.** A store nothing has written is version 0 and holds no
/// document.
///
/// The two halves are one test because they are one fact, and the `None` half
/// is the one with teeth: a backend that answered `Some(vec![])` for a store
/// nothing had written would tell the directory above it "there is a document,
/// and it is empty" — which compiles to a deployment with no tenancy at all,
/// silently, rather than to "compile the file alone".
pub async fn an_untouched_store_is_version_zero_and_holds_no_document<S: DocumentStore + ?Sized>(
    store: &S,
) {
    let loaded = store.load().await.expect("an empty store still loads");
    assert_eq!(
        loaded.version, 0,
        "version zero is the empty store, and every backend has to agree on \
         the number a caller compares against"
    );
    assert_eq!(
        loaded.document, None,
        "a store nothing has written holds no document -- answering empty \
         bytes instead would read as `there is a document and it says \
         nothing`, which is a different deployment"
    );
    assert_eq!(
        store.version().await.unwrap().version,
        0,
        "and the cheap read agrees with the expensive one"
    );
}

/// **The claim.** A commit against the version just read advances to the next
/// version, and the bytes come back exactly as they went in.
///
/// The byte equality is not decoration: this store is opaque, so nothing
/// downstream can repair a document a backend re-encoded. The non-UTF-8 byte
/// in [`document`] is what makes that assertion able to fail.
pub async fn a_commit_against_the_read_version_advances_it_and_the_bytes_come_back_exact<
    S: DocumentStore + ?Sized,
>(
    store: &S,
) {
    let read = store.load().await.unwrap();
    let written = document("first");
    let committed = store
        .commit(read.version, written.clone())
        .await
        .expect("a commit against the version just read is admitted");
    assert_eq!(
        committed.version,
        read.version + 1,
        "a commit advances by exactly one, which is what makes `has anything \
         changed` an integer comparison rather than a document read"
    );

    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.version, committed.version);
    assert_eq!(
        loaded.lineage, committed.lineage,
        "the identity a commit hands back is the identity the next reader \
         loads: a writer that was told one lineage and a reader that is told \
         another would each be right about a different store"
    );
    assert_eq!(
        loaded.document.as_deref(),
        Some(written.as_slice()),
        "the document is opaque, so a backend that re-encoded it -- through a \
         String, through a lossy decode -- has destroyed the only copy of the \
         deployment's tenancy"
    );
}

/// **The claim.** A commit against a version the store has moved past is
/// refused, the refusal names both versions, and the stored document is
/// untouched.
///
/// This is the whole reason the trait is compare-and-set rather than a write:
/// two nodes that each read version *n* and each wrote would leave one of the
/// two changes silently gone — a revocation overwritten by a concurrent
/// rename, which is exactly the state an admin plane must never reach. The
/// tail — that the refused write changed *nothing* — is the half a backend can
/// fail while still returning the right error, by writing the document and
/// then noticing.
pub async fn a_stale_commit_is_refused_naming_both_versions_and_changes_nothing<
    S: DocumentStore + ?Sized,
>(
    store: &S,
) {
    let kept = document("kept");
    let version = store.commit(0, kept.clone()).await.unwrap().version;

    let stale = store.commit(0, document("clobbering")).await;
    match stale {
        Err(DocumentStoreError::Concurrent { expected, found }) => {
            assert_eq!(expected, 0, "the refusal names the version the writer had");
            assert_eq!(
                found, version,
                "and the version the store is actually at, so a caller can see \
                 how far it has been overtaken rather than only that it has"
            );
        }
        other => panic!(
            "a commit against a version the store has moved past must answer \
             Concurrent naming both versions: {other:?}"
        ),
    }

    let loaded = store.load().await.unwrap();
    assert_eq!(
        loaded.version, version,
        "a refused commit must not have advanced the store"
    );
    assert_eq!(
        loaded.document.as_deref(),
        Some(kept.as_slice()),
        "nor replaced the document -- a backend that wrote first and compared \
         afterwards returns the right error and has already lost the write it \
         was protecting"
    );
}

/// **The claim.** `version()` tracks every commit without reading the
/// document.
///
/// The "without reading" half cannot be asserted from here — it is a claim
/// about round trips, pinned where round trips are countable, beside each
/// backend. What *is* asserted is the part a caller depends on: the cheap read
/// and the expensive one never disagree about the number, so a refresh that
/// skipped the load because `version()` had not moved did not skip a change.
pub async fn version_tracks_commits_without_reading_the_document<S: DocumentStore + ?Sized>(
    store: &S,
) {
    assert_eq!(store.version().await.unwrap().version, 0);

    let mut expected = 0;
    for round in 0..3 {
        expected = store
            .commit(expected, document(&format!("round-{round}")))
            .await
            .unwrap()
            .version;
        assert_eq!(
            store.version().await.unwrap().version,
            expected,
            "the cheap read must answer what the commit returned, or a node \
             that refreshes on `version` alone serves a plane the store has \
             already replaced"
        );
        assert_eq!(store.load().await.unwrap().version, expected);
    }
}

/// **The claim.** Committing bytes identical to the stored ones still advances
/// the version.
///
/// The version is a *write* counter and not a content hash (see the module
/// doc). A backend that compared documents and declined to advance would make
/// an admin call that happened to be a no-op — a rename to the same name, a
/// revocation replayed — invisible to every other node, because the only thing
/// those nodes look at is whether the number moved.
pub async fn identical_bytes_committed_again_still_advance_the_version<
    S: DocumentStore + ?Sized,
>(
    store: &S,
) {
    let same = document("unchanged");
    let first = store.commit(0, same.clone()).await.unwrap().version;
    let second = store.commit(first, same.clone()).await.unwrap().version;
    assert!(
        second > first,
        "an identical document committed again is still a write, and a store \
         that swallowed it would hide the write from every other node: {first} \
         then {second}"
    );
    assert_eq!(
        store.load().await.unwrap().document.as_deref(),
        Some(same.as_slice())
    );
}

/// **The claim.** Two commits racing against one version admit exactly one.
///
/// The contract's atomicity, stated as the thing the compare-and-set exists
/// for. Both futures are in flight at once against one store — over a
/// multiplexed connection for the Redis backend, so the two invocations really
/// do interleave at the server rather than being serialized by the client —
/// and the store must pick a winner: two successes means the check and the
/// write are not one step, and the loser's document has replaced the winner's
/// with nothing anywhere saying so.
///
/// **`tokio::join!` on one task, and that is a known, documented limit rather
/// than an oversight (M16.1 review, F2).** For the Redis backend the two
/// `commit` calls really do interleave — the server sees two requests on a
/// multiplexed connection and this test is exactly what pins "the Lua script
/// picks a winner" as a property of the wire, not of the client. For
/// `MemoryDocumentStore`, whose `commit` has no `.await` point at all, a
/// single-task join never gets the chance: the executor polls the first
/// future to `Ready` before it ever touches the second, so a Rust-side CAS
/// that split its compare and its write across two lock acquisitions would
/// sail through this assertion unnoticed. A version of this test that instead
/// raced real OS threads was built and verified against exactly that
/// mutation — up to 64 barrier-synchronized racers over 50 rounds — and it
/// still did not observe two winners, because `std::sync::Mutex` on Linux
/// favours a thread that just released a lock over any thread it woke to
/// contend for it, closing the gap in practice before another thread's first
/// read can land in it. That version was not kept: driving racer threads
/// through the ambient runtime's `Handle` starved the Redis instantiation's
/// own connection-driver task of the polls it needs whenever this async
/// function's own thread blocks synchronously waiting on the racers, timing
/// out every real commit in the suite that matters most. Closing the
/// `MemoryDocumentStore` gap without breaking the Redis one is a design
/// question — a model-checked tool like `loom`, or restructuring this family
/// out of the shared multi-backend list — left open rather than answered
/// unilaterally here; see the comment on
/// [`MemoryDocumentStore::commit`](super::MemoryDocumentStore::commit) for
/// the invariant a test does not currently enforce.
///
/// Which of the two wins is deliberately not asserted. A contract that named a
/// winner would be pinning a scheduling accident, and no caller can act on it:
/// the loser's next step is the same either way — read the current version and
/// try again.
pub async fn two_commits_racing_against_one_version_admit_exactly_one<S: DocumentStore + ?Sized>(
    store: &S,
) {
    let read = store.load().await.unwrap().version;
    let ours = document("ours");
    let theirs = document("theirs");

    let (first, second) = tokio::join!(
        store.commit(read, ours.clone()),
        store.commit(read, theirs.clone())
    );

    let winners = [&first, &second]
        .into_iter()
        .filter(|outcome| outcome.is_ok())
        .count();
    assert_eq!(
        winners, 1,
        "exactly one of two commits against one version may be admitted; two \
         successes means the compare and the write are not one step, and zero \
         means the store refused a writer that was not overtaken: {first:?} / \
         {second:?}"
    );

    let loaded = store.load().await.unwrap();
    assert_eq!(
        loaded.version,
        read + 1,
        "and the one admitted write advanced the version once, not twice"
    );
    let stored = loaded.document.expect("the winner's document is stored");
    assert!(
        stored == ours || stored == theirs,
        "the stored document is one of the two written, whole -- a store that \
         admitted one commit and kept the other's bytes has interleaved two \
         writes into a document neither writer produced"
    );
}

/// **The claim.** Versions strictly increase across every commit, whoever
/// commits and whatever they write — within one lineage, which is the whole
/// of the promise a durable backend can actually keep (see
/// [`one_lineage_spans_every_commit_and_every_read`]).
///
/// R-D2′ inherited (M16.0 review, F3). The directory above publishes by
/// comparing versions, so under a store whose numbers can repeat or fall,
/// "newer wins" silently means "the store's current truth loses, forever" —
/// a node's own `apply` returns success and drops its own write, and a key an
/// operator revoked stays live until restart.
///
/// Asserted across a run that mixes admitted and refused commits, because the
/// refusal path is where a naive implementation regresses: a backend that
/// returned the *found* version as if it were the new one would look monotone
/// under a suite that only ever committed successfully.
pub async fn versions_strictly_increase_across_commits<S: DocumentStore + ?Sized>(store: &S) {
    let mut seen = store.version().await.unwrap().version;
    for round in 0..4 {
        // A refused commit in between every admitted one. Its version must
        // not be handed back as a new version, and must not move the store.
        let refused = store.commit(seen + 99, document("never")).await;
        assert!(matches!(
            refused,
            Err(DocumentStoreError::Concurrent { .. })
        ));
        assert_eq!(store.version().await.unwrap().version, seen);

        let next = store
            .commit(seen, document(&format!("advance-{round}")))
            .await
            .unwrap()
            .version;
        assert!(
            next > seen,
            "every commit returns a version strictly greater than any this \
             store has handed out: had {seen}, got {next}"
        );
        seen = next;
    }
}

/// **The claim.** One lineage spans every commit and every read, and all three
/// methods agree about it.
///
/// R-D2″ (M16.1 review, F1). The lineage is what a reader compares to catch a
/// counter that restarted, so it is worth exactly nothing if it moves on its
/// own: a backend that minted a fresh one per commit, or per handle, or per
/// read, would make every ordinary write look like a store that had lost its
/// key, and the reader above would reload and warn once per admin call
/// forever. The lost-key case itself is not assertable from here — no backend
/// can lose its key on request through this trait, which is why the delete
/// case lives beside the Redis instantiation, where a raw `DEL` is available.
///
/// The empty store is deliberately not part of the claim. Version zero means
/// nothing has been written, so there is no document for a lineage to be
/// about, and a durable backend whose key does not exist yet has nothing to
/// answer with; the lineage becomes meaningful with the first commit, which is
/// where this starts looking.
pub async fn one_lineage_spans_every_commit_and_every_read<S: DocumentStore + ?Sized>(store: &S) {
    let first = store.commit(0, document("first")).await.unwrap();
    assert!(
        !first.lineage.is_empty(),
        "a store holding a document has a lineage to name it by; an empty one \
         cannot be compared against anything and would make every reader's \
         check vacuous"
    );

    assert_eq!(
        store.load().await.unwrap().lineage,
        first.lineage,
        "the expensive read agrees with the commit that produced it"
    );
    assert_eq!(
        store.version().await.unwrap().lineage,
        first.lineage,
        "and so does the cheap one -- the reader's refresh asks `version` \
         first, so a lineage that only appeared on the load path would be a \
         lineage the refresh never checks"
    );

    let second = store
        .commit(first.version, document("second"))
        .await
        .unwrap();
    assert_eq!(
        second.lineage, first.lineage,
        "an ordinary write is the same deployment's key, still there: a \
         backend that minted a new lineage per commit would make every admin \
         call indistinguishable from a flushed store"
    );
    assert!(second.version > first.version);
    assert_eq!(store.load().await.unwrap().lineage, first.lineage);
}

/// **The claim.** A document of a few megabytes survives the round trip whole.
///
/// A deployment's whole tenancy is one value here, so "how big can it get" is
/// not a hypothetical: a few thousand keys with their memberships is
/// megabytes of JSON, and the failure mode of a backend that cannot take one
/// is not a clean error — it is a document truncated at some protocol limit
/// and a plane compiled from half a deployment's keys.
///
/// Deliberately incompressible-ish and byte-varying rather than one repeated
/// character, so a backend that silently compressed or de-duplicated would
/// still have to reproduce the exact bytes.
pub async fn a_multi_megabyte_document_round_trips<S: DocumentStore + ?Sized>(store: &S) {
    const SIZE: usize = 3 * 1024 * 1024;
    let big: Vec<u8> = (0..SIZE).map(|index| (index % 251) as u8).collect();

    let version = store
        .commit(0, big.clone())
        .await
        .expect(
            "a store that cannot take a few megabytes cannot hold a real \
             deployment's tenancy",
        )
        .version;
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.version, version);
    let stored = loaded.document.expect("the big document is stored");
    assert_eq!(
        stored.len(),
        big.len(),
        "a truncated document compiles to a plane with half the deployment's \
         keys in it, which admits and refuses the wrong callers silently"
    );
    assert!(
        stored == big,
        "and every byte of it is the byte that went in"
    );
}

/// Instantiate the whole conformance suite against one backend.
///
/// The single list of contract tests, in the same idiom and for the same
/// reason as
/// [`fair_use_ledger_contract_suite!`](crate::fair_use_ledger_contract_suite):
/// a backend gets the entire suite in one call, so there is no per-test wiring
/// step where one test can be forgotten for one backend while the others keep
/// enforcing it.
///
/// `$make` is evaluated inside each generated test, so every test gets a fresh
/// handle — which for this family is not a convenience but the isolation
/// itself (see the module doc: one key per namespace, so a Redis
/// instantiation's `$make` mints a fresh namespace). The optional
/// `ignore = "…"` prefix stamps that reason as `#[ignore]` on every generated
/// test, which is how an infrastructure-gated backend applies its gate
/// suite-wide.
///
/// ```ignore
/// roundhouse_core::document_store_contract_suite!(MemoryDocumentStore::new());
///
/// roundhouse_core::document_store_contract_suite!(
///     ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
///     connect_in_a_fresh_namespace().await,
/// );
/// ```
///
/// Only usable where the `contract` module is compiled: this crate's own
/// tests, or a dependent with the `test-support` feature on its
/// dev-dependency.
#[macro_export]
macro_rules! document_store_contract_suite {
    (ignore = $reason:literal, $make:expr $(,)?) => {
        $crate::document_store_contract_suite!(@list (#[ignore = $reason]) $make);
    };
    ($make:expr $(,)?) => {
        $crate::document_store_contract_suite!(@list () $make);
    };
    // The single list. Both public arms land here, so gated and ungated
    // backends cannot drift apart in coverage. The recursion that turns this
    // list into one `#[tokio::test]` per name is
    // [`__contract_suite!`](crate::__contract_suite), shared with the other
    // four families (M14.1 review, F6).
    (@list $attrs:tt $make:expr) => {
        $crate::__contract_suite!(store, $crate::control::directory::contract, $attrs, $make;
            an_untouched_store_is_version_zero_and_holds_no_document,
            a_commit_against_the_read_version_advances_it_and_the_bytes_come_back_exact,
            a_stale_commit_is_refused_naming_both_versions_and_changes_nothing,
            version_tracks_commits_without_reading_the_document,
            identical_bytes_committed_again_still_advance_the_version,
            two_commits_racing_against_one_version_admit_exactly_one,
            versions_strictly_increase_across_commits,
            one_lineage_spans_every_commit_and_every_read,
            a_multi_megabyte_document_round_trips,
        );
    };
}
