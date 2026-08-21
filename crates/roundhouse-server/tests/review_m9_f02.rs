// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! F02 (thermo-nuclear review, M9): `codex_e2e.rs`'s `Rig::assert_never_forked`
//! builds the id it checks for from `Rig::session()` — which is
//! `Conversations::latest(principal)` — *after* `fork()` has already moved
//! `latest` to the forked id. Appending `#g1` to an id that is already
//! `key#g1` asks the store about `key#g1#g1`, a session nothing ever creates,
//! so the guard reads "never forked" whether or not a fork happened.
//!
//! This reproduces the guard's exact expression against `Conversations` and
//! `MemoryStore` directly — no `codex` binary, no socket, no `e2e-codex`
//! feature — because the defect is in the id arithmetic, not in anything a
//! real client does.

use roundhouse_core::control::Principal;
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_server::Conversations;

/// Mirrors `codex_e2e.rs`'s `Rig::session()`: the production accessor for
/// "the last session this principal drove a turn on".
fn session(conversations: &Conversations, principal: &Principal) -> SessionId {
    conversations
        .latest(principal)
        .expect("a turn has bound this principal to a session")
}

/// Mirrors `codex_e2e.rs`'s `Rig::assert_never_forked()` verbatim, except it
/// *returns* the guard's verdict instead of asserting it inline — so this
/// test can check that verdict against ground truth rather than only being
/// able to observe whether `assert!` happened to panic.
///
/// `true` means "the guard believes no fork happened" — the same condition
/// `codex_e2e.rs` requires to be true unconditionally via `assert!`.
async fn guard_believes_no_fork(
    store: &MemoryStore,
    conversations: &Conversations,
    principal: &Principal,
) -> bool {
    let forked = SessionId::new(format!("{}#g1", session(conversations, principal)));
    store.last_seq(&forked).await.is_err()
}

#[tokio::test]
#[ignore = "F02: assert_never_forked() derives the id it probes from Rig::session(), which \
            Conversations::latest() already updated to the forked id by the time the guard \
            runs. It probes `key#g1#g1` (never created) instead of the real fork's `key#g1` \
            (which the store does hold), so it reads green on a session that DID fork. \
            Test-first per CLAUDE.md; fix belongs to the codex_e2e stage, not this refuter."]
async fn f02_assert_never_forked_misses_a_real_fork_because_session_already_reflects_it() {
    let principal = Principal::new("acme", "ada");
    let key = "acme/ada/main";
    let conversations = Conversations::new();
    let store = MemoryStore::new();

    // Turn 1: an ordinary bind at generation zero, exactly as
    // `responses_api::bind` records "this principal is working in this
    // session" before the first turn runs.
    let gen0 = conversations.bind(&principal, key);
    store
        .create_session(&gen0, "policy")
        .await
        .expect("generation zero is fresh");

    // Turn 2: the client's resend disagreed with the log, so
    // `responses_api` calls `Conversations::fork`, which — per
    // conversations.rs:117-123 — both mints `key#g1` AND updates `latest`
    // to it before this test (or the real guard) ever asks. This is the
    // actual fork the finding says the guard must catch.
    let real_fork = conversations.fork(&principal, key);
    assert_eq!(real_fork.as_str(), "acme/ada/main#g1");
    store
        .create_session(&real_fork, "policy")
        .await
        .expect("the fork's session is newly created");

    // Control: the store's own ground truth agrees a fork happened. If this
    // assertion ever failed, the test would be exercising the wrong thing,
    // not proving F02.
    assert!(
        store.last_seq(&real_fork).await.is_ok(),
        "control failed: the real fork's session must exist in the store"
    );

    // The guard, evaluated exactly as `codex_e2e.rs` evaluates it after a
    // turn: `assert!(store.last_seq(&forked).await.is_err(), ...)`, where
    // `forked` is built from `self.session()`. A real fork happened, so a
    // correct guard's verdict must be "not clean" — this must be `false`.
    let clean = guard_believes_no_fork(&store, &conversations, &principal).await;
    assert!(
        !clean,
        "assert_never_forked's own arithmetic hid a real fork: it probed \
         `{}#g1` (session() already reflects the fork), found nothing, and \
         called that clean — even though `{real_fork}` exists in the store as \
         proof the fork actually happened",
        session(&conversations, &principal),
    );
}
