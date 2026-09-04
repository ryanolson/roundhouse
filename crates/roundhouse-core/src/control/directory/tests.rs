// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`MemoryDocumentStore`] against the contract it is the specification of,
//! plus the two facts that are about *this* implementation rather than about
//! the seam.

use super::*;

crate::document_store_contract_suite!(MemoryDocumentStore::new());

/// A store committed *empty bytes* is not the empty store, and the two answers
/// are visibly different.
///
/// The distinction the [`VersionedDocument`] doc draws, driven rather than
/// asserted in prose: `None` at version 0 means "nothing has ever written
/// tenancy here, compile the file alone", and `Some(vec![])` at version 1
/// means "a document exists and it happens to be empty". A store that
/// collapsed the two would turn a deployment whose admin plane had genuinely
/// been emptied into one that had never been configured — and on the next
/// boot, the file's own entries would be the whole plane with no line
/// anywhere saying an admin document had been passed over.
///
/// Not in the contract because it constrains a *caller* rather than a backend:
/// the contract's first assertion already pins the empty-store half, and no
/// backend can produce the second half without a caller deliberately
/// committing zero bytes.
#[tokio::test]
async fn a_document_of_zero_bytes_is_not_the_absence_of_a_document() {
    let store = MemoryDocumentStore::new();
    assert_eq!(store.load().await.unwrap().document, None);

    store.commit(0, Vec::new()).await.unwrap();
    let loaded = store.load().await.unwrap();
    assert_eq!(
        loaded.document,
        Some(Vec::new()),
        "a document that was written and says nothing must not read back as \
         a document that was never written"
    );
    assert_eq!(loaded.version, 1);
}

/// A fresh store is a new lineage; one that continues a lineage says so.
///
/// The memory half of R-D2″ (M16.1 review, F1). This store cannot lose a key
/// and keep answering, so "the counter restarted" has exactly one shape here:
/// a *different store*. Pinning it is what makes the fixture above it able to
/// tell a neighbour node's ordinary write ([`MemoryDocumentStore::continuing`],
/// same lineage, higher version) from a restored backup (a fresh store, a new
/// lineage, whatever version) — the two cases the durable backend has to be
/// able to distinguish and could not, before the lineage existed.
#[tokio::test]
async fn a_fresh_store_is_a_new_lineage_and_continuing_one_is_not() {
    let first = MemoryDocumentStore::new();
    let second = MemoryDocumentStore::new();
    assert_ne!(
        first.commit(0, b"one".to_vec()).await.unwrap().lineage,
        second.commit(0, b"two".to_vec()).await.unwrap().lineage,
        "two stores that each answered version 1 for different documents must \
         not claim to be the same run of one counter"
    );

    let continuing = MemoryDocumentStore::continuing(first.lineage());
    assert_eq!(
        continuing
            .commit(0, b"three".to_vec())
            .await
            .unwrap()
            .lineage,
        first.lineage(),
        "a store told which lineage it continues answers that one -- what a \
         fixture rebuilding a store at some version means by `the same \
         deployment's key, still there`"
    );
}

/// A poisoned lock does not take the store down with it.
///
/// The recovery the implementation's comment argues for, exercised: a panic
/// inside a `commit` poisons the `Mutex`, and every later call still answers.
/// Without the `unwrap_or_else(into_inner)`, one panicking admin request would
/// refuse every admission for the life of the process — a far worse failure
/// than the half-written byte string that cannot happen here, because the
/// state behind this lock is a `Vec<u8>` and a counter rather than an
/// invariant spanning two fields.
#[tokio::test]
async fn a_poisoned_lock_still_answers() {
    let store = std::sync::Arc::new(MemoryDocumentStore::new());
    store.commit(0, b"before".to_vec()).await.unwrap();

    let poisoner = std::sync::Arc::clone(&store);
    let panicked = std::thread::spawn(move || {
        let _guard = poisoner.state.lock().unwrap();
        panic!("a request panicked while holding the store lock");
    })
    .join();
    assert!(panicked.is_err(), "the helper thread must actually panic");

    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.document.as_deref(), Some(b"before".as_slice()));
    assert_eq!(store.version().await.unwrap().version, 1);
    assert_eq!(store.commit(1, b"after".to_vec()).await.unwrap().version, 2);
}
