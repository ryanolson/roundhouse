// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2, R-S3: two deployments sharing one Redis under different namespaces
//! cannot see each other's keys — for every family.
//!
//! Each test connects two handles of one family to the *same* Redis under
//! namespaces `"a"` and `"b"`, writes through one, and shows the other cannot
//! read it — not merely that it answers differently, but that it answers
//! exactly as it would for a key nothing ever wrote, which is what "cannot
//! see" has to mean for a family whose read path has no "wrong tenant"
//! answer of its own.
//!
//! Gated like every other file in this crate's `tests/`: `#[ignore]`, opted
//! into with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL`
//! fails loudly rather than skipping quietly.

use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{
    Allocation, Balance, BalanceQuery, Budget, BudgetTerms, BudgetWindow, CorrelationMaps,
    Exhaustion, FairUseLedger, FairUseLimit, FairUseTerms, FairUseWindow, GrantRequest,
    SpendLedger,
};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::store::SessionStore;
use roundhouse_store_redis::test_support::url_from_env;
use roundhouse_store_redis::{
    KeyNamespace, RedisCorrelationMaps, RedisFairUseLedger, RedisSessionStore, RedisSpendLedger,
};

fn namespace(raw: &str) -> KeyNamespace {
    KeyNamespace::new(raw).expect("a non-empty literal is always accepted")
}

/// **Sessions and their leases.** A session created under `"a"` is invisible
/// under `"b"` — not merely leaseless, but *absent*, which is the same
/// `SessionNotFound` a session id nobody ever created answers with.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_session_created_under_one_namespace_does_not_exist_under_another() {
    let url = url_from_env();
    let a = RedisSessionStore::connect_namespaced(&url, namespace("a"))
        .await
        .expect("the test Redis must be reachable");
    let b = RedisSessionStore::connect_namespaced(&url, namespace("b"))
        .await
        .expect("the test Redis must be reachable");

    let session = SessionId::generate();
    assert!(
        a.create_session(&session, "policy").await.unwrap(),
        "the session must be newly created under namespace a"
    );

    let leased = b.acquire_lease(&session, "node-b", 30_000).await;
    assert!(
        matches!(
            leased,
            Err(roundhouse_core::store::StoreError::SessionNotFound(_))
        ),
        "namespace b must answer exactly as it would for a session id \
         nobody ever created, not merely \"unleased\" — got {leased:?}"
    );

    // CONTROL: the same session, read back through the namespace that
    // created it, exists and can be leased.
    assert!(
        a.acquire_lease(&session, "node-a", 30_000)
            .await
            .unwrap()
            .is_some()
    );
}

/// **The spend ledger.** A grant opened under `"a"` leaves no committed
/// spend visible under `"b"`.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_grant_opened_under_one_namespace_is_invisible_under_another() {
    let url = url_from_env();
    let a = RedisSpendLedger::connect_namespaced(&url, namespace("a"))
        .await
        .expect("the test Redis must be reachable");
    let b = RedisSpendLedger::connect_namespaced(&url, namespace("b"))
        .await
        .expect("the test Redis must be reachable");

    let principal = fresh_principal("ada");
    let terms = BudgetTerms {
        budget: Budget {
            limit_usd: 100.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    };

    a.open_grant(GrantRequest {
        principal: principal.clone(),
        session_id: SessionId::generate(),
        response_id: ResponseId::new("namespace-isolation-a"),
        requested_usd: 10.0,
        ttl_ms: 60_000,
        terms: terms.clone(),
        now_ms: 0,
    })
    .await
    .unwrap();

    let seen_from_b: Balance = b
        .balance(BalanceQuery {
            principal,
            terms,
            now_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(
        seen_from_b.committed_usd, 0.0,
        "a grant opened under namespace a must not be visible as committed \
         or held spend under namespace b"
    );
    assert_eq!(seen_from_b.held_usd, 0.0);
}

/// **Fair use.** A draw recorded under `"a"` does not count toward a
/// ceiling checked under `"b"` — proved by a cap so tight that the draw, if
/// visible, would certainly trip it.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_draw_recorded_under_one_namespace_does_not_count_under_another() {
    let url = url_from_env();
    let a = RedisFairUseLedger::connect_namespaced(&url, namespace("a"))
        .await
        .expect("the test Redis must be reachable");
    let b = RedisFairUseLedger::connect_namespaced(&url, namespace("b"))
        .await
        .expect("the test Redis must be reachable");

    let principal = fresh_principal("ada");
    let terms = FairUseTerms {
        project: vec![FairUseLimit {
            window: FairUseWindow::FiveHours,
            max_tokens: Some(1),
            max_usd: None,
        }],
        member: Vec::new(),
    };

    a.record_draw(&principal, 0, 100, 0.0).await.unwrap();

    let refused_under_b = b.would_exceed(&principal, &terms, 0).await.unwrap();
    assert!(
        refused_under_b.is_none(),
        "a 100-token draw under namespace a must not be counted toward a \
         1-token cap checked under namespace b — got {refused_under_b:?}"
    );

    // CONTROL: the same draw, checked back through the namespace it was
    // recorded under, does trip the cap.
    let refused_under_a = a.would_exceed(&principal, &terms, 0).await.unwrap();
    assert!(refused_under_a.is_some());
}

/// **Correlation.** A generation committed, a call bound and a thread bound
/// under `"a"` are all absent under `"b"` — the M12.1 F9 refusal, now proven
/// across namespaces the way `correlation_contract.rs` proves it across
/// nodes.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn correlation_bound_under_one_namespace_is_absent_under_another() {
    let url = url_from_env();
    let a = RedisCorrelationMaps::connect_namespaced(&url, namespace("a"))
        .await
        .expect("the test Redis must be reachable");
    let b = RedisCorrelationMaps::connect_namespaced(&url, namespace("b"))
        .await
        .expect("the test Redis must be reachable");

    let ada = fresh_principal("ada");
    let key = format!("acme/ada/namespace-isolation-{}", uuid::Uuid::new_v4());
    let session = SessionId::new("acme/ada/main");

    a.set_generation(&key, 3).await.unwrap();
    a.bind_call(&ada, "toolu_ns_isolation", &session)
        .await
        .unwrap();
    a.bind_thread(&ada, "thread-ns-isolation", &session)
        .await
        .unwrap();

    assert_eq!(
        b.generation(&key).await.unwrap(),
        None,
        "a generation committed under namespace a must be absent under b"
    );
    assert_eq!(
        b.session_of_call(&ada, "toolu_ns_isolation").await.unwrap(),
        None
    );
    assert_eq!(
        b.session_of_thread(&ada, "thread-ns-isolation")
            .await
            .unwrap(),
        None
    );

    // CONTROL: the same three, read back through the namespace that wrote
    // them, all answer.
    assert_eq!(a.generation(&key).await.unwrap(), Some(3));
    assert_eq!(
        a.session_of_call(&ada, "toolu_ns_isolation").await.unwrap(),
        Some(session.clone())
    );
    assert_eq!(
        a.session_of_thread(&ada, "thread-ns-isolation")
            .await
            .unwrap(),
        Some(session)
    );
}
