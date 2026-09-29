// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Seeding a real session store from a crafted log, the way `Session::commit`
//! writes one: every event appended under a lease, with the learning mark
//! `learning_mark` computes for its batch.
//!
//! One event per batch, so the mark each append carries is the one the
//! session state before it calls for. The store stamps its own `at_ms`, so a
//! seeded log keeps the crafted order and content but not the crafted times.

#![allow(dead_code)]

use roundhouse_core::routing::CacheLedger;
use roundhouse_core::session::{SessionState, learning_mark};
use roundhouse_core::store::SessionStore;

use super::learning_support::Script;

pub async fn seed<S: SessionStore>(store: &S, script: &Script) {
    store
        .create_session(&script.session, "stage")
        .await
        .expect("a new session");
    let lease = store
        .acquire_lease(&script.session, "seeder", 60_000)
        .await
        .expect("the store answers")
        .expect("an unleased session");
    for event in &script.events {
        let state = SessionState::project(store, &script.session, CacheLedger::new(), None)
            .await
            .expect("the seeded log replays");
        let kinds = vec![event.kind.clone()];
        let mark = learning_mark(&state, &kinds);
        store
            .append_events(&lease, kinds, mark)
            .await
            .expect("the seeder holds the lease");
    }
    store.release_lease(&lease).await.expect("released");
}
