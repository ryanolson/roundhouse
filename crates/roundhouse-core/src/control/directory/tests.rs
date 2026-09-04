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
    assert_eq!(store.version().await.unwrap(), 1);
    assert_eq!(store.commit(1, b"after".to_vec()).await.unwrap(), 2);
}
