// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M3: the full [`SpendLedger`] contract against a real Redis, plus the
//! adversarial cases only a real backend can exercise.
//!
//! The macro invocation is the milestone's headline, in the same idiom as
//! `contract.rs`: the *same* eleven assertions that judge `MemorySpendLedger`
//! now judge this store. `the_project_and_member_keys_share_one_hash_tag`
//! needs no live Redis — key strings are pure formatting — so it lives as a
//! unit test beside `account_key`/`holds_key`/`watermarks_key` in
//! `src/spend.rs` instead of being duplicated here as an ignore-gated test
//! that would only add a dependency on infrastructure it does not need.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

mod common;

use roundhouse_core::control::{
    Allocation, Balance, BalanceQuery, Budget, BudgetTerms, BudgetWindow, Exhaustion, GrantRequest,
    MemorySpendLedger, Principal, ProjectId, Settlement, SpendLedger,
};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_store_redis::RedisSpendLedger;
use roundhouse_store_redis::test_support::{spend_holds_key, url_from_env};

roundhouse_core::spend_ledger_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_spend_from_env().await
);

async fn connect_spend_from_env() -> RedisSpendLedger {
    RedisSpendLedger::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable")
}

async fn raw_from_env() -> redis::aio::MultiplexedConnection {
    redis::Client::open(url_from_env().as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
}

/// A membership nothing else in this file shares, mirroring
/// `roundhouse_core::control::spend::contract::fresh_principal` — private to
/// that module, so this is the second, deliberately identical, copy.
fn fresh_principal(user: &str) -> Principal {
    Principal::new(
        ProjectId::new(format!("proj_{}", uuid::Uuid::new_v4().simple())),
        user,
    )
}

fn pooled_terms(limit_usd: f64) -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    }
}

/// Dollars compare to the cent, not to the bit — the same tolerance the
/// shared contract suite uses, for the same reason: Redis accumulates
/// through Lua's doubles, which need not round identically to Rust's on
/// every intermediate step.
#[track_caller]
fn assert_usd(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-6,
        "{what}: expected ${expected}, got ${actual}"
    );
}

/// A hold whose process died before settling: nobody calls `settle_grant`
/// for it, ever. The crash story says this self-heals within one TTL,
/// through whichever call happens to look next — proven here by reading the
/// raw Redis hash, not just the numbers the trait reports, so the assertion
/// is about the actual storage mechanism and not only its derived balance
/// (which `a_held_grant_is_released_once_its_ttl_lapses`, part of the shared
/// suite above, already covers).
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_hold_whose_owner_died_is_expired_lazily_by_the_next_grant() {
    let ledger = connect_spend_from_env().await;
    let mut raw = raw_from_env().await;
    let principal = fresh_principal("ada");
    let terms = pooled_terms(10.0);
    let session = SessionId::new(format!("sess_{}", principal.user));
    let holds_key = spend_holds_key(&principal.project);

    ledger
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: session.clone(),
            response_id: ResponseId::new("abandoned"),
            requested_usd: 10.0,
            ttl_ms: 1_000,
            terms: terms.clone(),
            now_ms: 0,
        })
        .await
        .unwrap();

    let exists_before: bool = redis::cmd("HEXISTS")
        .arg(&holds_key)
        .arg("abandoned")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        exists_before,
        "the hold is a real field in the holds hash while its owner is presumed alive"
    );

    // Nobody ever settles it. A later grant — a different response, standing
    // in for whichever process happens to open the next turn under this
    // project — is what lazily expires it; nothing sweeps.
    let later = ledger
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: session,
            response_id: ResponseId::new("next"),
            requested_usd: 10.0,
            ttl_ms: 60_000,
            terms,
            now_ms: 2_000, // past the abandoned hold's TTL
        })
        .await
        .unwrap();
    assert_usd(
        later.granted_usd,
        10.0,
        "the whole budget, once the dead hold is gone",
    );

    let exists_after: bool = redis::cmd("HEXISTS")
        .arg(&holds_key)
        .arg("abandoned")
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        !exists_after,
        "the abandoned hold's field is deleted, not merely excluded from the sum"
    );
}

/// Every dollar amount travelling across the Lua boundary as a fixed-decimal
/// string (documented in `src/spend/scripts.rs`) is what makes this safe:
/// this test exercises a value the Redis integer-truncation bug would have
/// mangled on the very first call.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_sub_dollar_grant_is_not_truncated_to_zero() {
    let ledger = connect_spend_from_env().await;
    let principal = fresh_principal("ada");
    let terms = pooled_terms(1.0);

    let grant = ledger
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: SessionId::new(format!("sess_{}", principal.user)),
            response_id: ResponseId::new("r1"),
            requested_usd: 0.35,
            ttl_ms: 60_000,
            terms,
            now_ms: 0,
        })
        .await
        .unwrap();
    assert_usd(
        grant.granted_usd,
        0.35,
        "a fraction of a dollar, not truncated by a Lua number reply",
    );
}

/// Counts `EVAL`/`EVALSHA` calls between two `CONFIG RESETSTAT`-bounded
/// points on the *server*, since the `redis` crate's `ConnectionManager`
/// exposes no per-call counter of its own — and takes the minimum over
/// several attempts rather than trusting any single one, because that
/// server-wide counter is shared with every other test in this binary
/// running concurrently against the same Redis. See the comment at the
/// first measurement loop for why the minimum is still the right answer.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn open_grant_and_settle_grant_are_single_round_trips() {
    let ledger = connect_spend_from_env().await;
    let mut raw = raw_from_env().await;
    let principal = fresh_principal("ada");
    let terms = pooled_terms(1_000.0);
    let session = SessionId::new(format!("sess_{}", principal.user));

    async fn eval_calls_since_reset(raw: &mut redis::aio::MultiplexedConnection) -> u64 {
        let info: String = redis::cmd("INFO")
            .arg("commandstats")
            .query_async(raw)
            .await
            .unwrap();
        info.lines()
            .filter_map(|line| {
                line.strip_prefix("cmdstat_eval:calls=")
                    .or_else(|| line.strip_prefix("cmdstat_evalsha:calls="))
            })
            .filter_map(|rest| rest.split(',').next())
            .filter_map(|n| n.parse::<u64>().ok())
            .sum()
    }

    async fn reset_stats(raw: &mut redis::aio::MultiplexedConnection) {
        let _: () = redis::cmd("CONFIG")
            .arg("RESETSTAT")
            .query_async(raw)
            .await
            .unwrap();
    }

    // `INFO commandstats` is server-wide, so a shared test Redis makes any
    // *single* measurement unreliable: another test's own script call can
    // land in the sliver of time between `CONFIG RESETSTAT` and the `INFO`
    // read around it. That noise is one-directional — it can only add calls
    // this test did not make, never hide the one it did — so the true answer
    // is the minimum across several independent attempts: the attempt that
    // happened not to race anyone, which a handful of tries makes
    // overwhelmingly likely to occur at least once.
    const ATTEMPTS: u64 = 10;

    let mut min_grant_calls = u64::MAX;
    for attempt in 0..ATTEMPTS {
        reset_stats(&mut raw).await;
        ledger
            .open_grant(GrantRequest {
                principal: principal.clone(),
                session_id: session.clone(),
                response_id: ResponseId::new(format!("grant-probe-{attempt}")),
                requested_usd: 1.0,
                ttl_ms: 60_000,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        min_grant_calls = min_grant_calls.min(eval_calls_since_reset(&mut raw).await);
    }
    assert_eq!(
        min_grant_calls, 1,
        "open_grant checks and debits both ceilings in one script"
    );

    let mut min_settle_calls = u64::MAX;
    for attempt in 0..ATTEMPTS {
        // Opening the grant is outside the measurement window; only the
        // settle itself is timed.
        ledger
            .open_grant(GrantRequest {
                principal: principal.clone(),
                session_id: session.clone(),
                response_id: ResponseId::new(format!("settle-probe-{attempt}")),
                requested_usd: 1.0,
                ttl_ms: 60_000,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();

        reset_stats(&mut raw).await;
        ledger
            .settle_grant(Settlement {
                principal: principal.clone(),
                session_id: session.clone(),
                seq: attempt + 1,
                response_id: ResponseId::new(format!("settle-probe-{attempt}")),
                actual_usd: 0.5,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        min_settle_calls = min_settle_calls.min(eval_calls_since_reset(&mut raw).await);
    }
    assert_eq!(
        min_settle_calls, 1,
        "settle_grant releases the hold and applies the spend in one script"
    );
}

/// The same grant/settle op log — two members' grants, an idempotent settle
/// replay, and one settle that overcommits past its hold — replayed through
/// `MemorySpendLedger` and this Redis backend, asserting the two report the
/// identical balance afterward. This is the portability argument the shared
/// contract suite makes per-assertion, made once more end to end: two
/// implementations, one Rust and one Lua, agreeing on a whole sequence
/// rather than one call at a time.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_two_backends_agree_on_a_grant_settle_replay_sequence() {
    async fn run<L: SpendLedger>(
        ledger: &L,
        ada: &Principal,
        bob: &Principal,
        session_a: &SessionId,
        session_b: &SessionId,
        terms: &BudgetTerms,
    ) -> Balance {
        ledger
            .open_grant(GrantRequest {
                principal: ada.clone(),
                session_id: session_a.clone(),
                response_id: ResponseId::new("r1"),
                requested_usd: 6.0,
                ttl_ms: 60_000,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        ledger
            .open_grant(GrantRequest {
                principal: bob.clone(),
                session_id: session_b.clone(),
                response_id: ResponseId::new("r2"),
                requested_usd: 6.0,
                ttl_ms: 60_000,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        ledger
            .settle_grant(Settlement {
                principal: ada.clone(),
                session_id: session_a.clone(),
                seq: 1,
                response_id: ResponseId::new("r1"),
                actual_usd: 2.0,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        // The replay case: the same (session, seq) settled again must be a
        // no-op on both backends.
        ledger
            .settle_grant(Settlement {
                principal: ada.clone(),
                session_id: session_a.clone(),
                seq: 1,
                response_id: ResponseId::new("r1"),
                actual_usd: 2.0,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        // The overcommit case: settling above the hold.
        ledger
            .settle_grant(Settlement {
                principal: bob.clone(),
                session_id: session_b.clone(),
                seq: 1,
                response_id: ResponseId::new("r2"),
                actual_usd: 9.0,
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap();
        ledger
            .balance(BalanceQuery {
                principal: ada.clone(),
                terms: terms.clone(),
                now_ms: 0,
            })
            .await
            .unwrap()
    }

    let ada = fresh_principal("ada");
    let bob = Principal::new(ada.project.clone(), "bob");
    let session_a = SessionId::new(format!("sess_{}", ada.user));
    let session_b = SessionId::new(format!("sess_{}", bob.user));
    let terms = pooled_terms(10.0);

    let from_memory = run(
        &MemorySpendLedger::new(),
        &ada,
        &bob,
        &session_a,
        &session_b,
        &terms,
    )
    .await;
    let from_redis = run(
        &connect_spend_from_env().await,
        &ada,
        &bob,
        &session_a,
        &session_b,
        &terms,
    )
    .await;

    assert_usd(
        from_memory.committed_usd,
        from_redis.committed_usd,
        "project committed spend",
    );
    assert_usd(from_memory.held_usd, from_redis.held_usd, "live holds");
    assert_usd(
        from_memory.project_remaining_usd,
        from_redis.project_remaining_usd,
        "project remaining",
    );
    assert_usd(
        from_memory.member_committed_usd,
        from_redis.member_committed_usd,
        "ada's committed spend",
    );
    assert_eq!(
        from_memory.state, from_redis.state,
        "the two backends must agree the project is exhausted: $11 committed on a $10 limit"
    );
}
