// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The sixth family, against a real Redis: evaluation spend is a *different*
//! ledger from serving spend, and a deployment's evaluation keys are not another
//! deployment's serving keys.
//!
//! **This is the arm where both claims can fail.** In the per-process arm the
//! two ledgers are two `MemorySpendLedger` values and are distinct by
//! construction, so a unit test there proves the type system rather than the
//! wiring. In the shared arm they are two `RedisSpendLedger`s against one
//! server, distinct only because their keys are.
//!
//! The collision the first draft had is the second test below. Deriving the
//! evaluation namespace as `<ns>-eval` made deployment `tenant`'s evaluation
//! ledger *the same keys* as deployment `tenant-eval`'s serving ledger — two
//! tenants sharing one counter, and the only symptom would have been one of them
//! being refused turns it had budget for. `SpendPurpose` puts the distinction
//! inside the deployment's own namespace instead, where it cannot reach a
//! namespace an operator chose.
//!
//! Asserted through [`open`] rather than by building ledgers here, for the
//! reason `shared_backend`'s own module doc gives about M14.1's F1: a test that
//! re-derives the wiring proves nothing about the wiring a deployment gets.
//!
//! Gated like the store's own integration tests: `#[ignore]`, opted into with
//! `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` fails loudly.

use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{
    Allocation, BalanceQuery, Budget, BudgetTerms, BudgetWindow, Exhaustion, GrantRequest,
    Principal, Settlement, SettlementKey, SpendLedger,
};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_server::Backends;
use roundhouse_server::shared_backend::{open, resolve_namespace};
use roundhouse_store_redis::test_support::url_from_env;
use roundhouse_store_redis::{KeyNamespace, RedisSpendLedger, SpendPurpose};

fn terms() -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd: 100.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::Refuse,
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    }
}

/// A namespace no other run of this suite uses.
///
/// The assertions are about what is *in* Redis, and the provisioned instance is
/// shared across runs: a fixed namespace would let an earlier green run's
/// committed dollars make a later run pass without the settle it claims.
fn unique_namespace(suffix: &str) -> KeyNamespace {
    let id = uuid::Uuid::new_v4().simple().to_string();
    KeyNamespace::new(format!("t{}{suffix}", &id[..12])).expect("a legal namespace")
}

/// Grant and settle `usd` for `principal` through `ledger`.
async fn spend(ledger: &dyn SpendLedger, principal: &Principal, call: &str, usd: f64) {
    let call_id = ResponseId::new(call);
    let grant = ledger
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: SessionId::new("sess_evaluation_ledger_boot"),
            response_id: call_id.clone(),
            requested_usd: usd,
            ttl_ms: 60_000,
            terms: terms(),
            now_ms: 1_000,
        })
        .await
        .expect("the ledger grants");
    assert_eq!(grant.granted_usd, usd);
    ledger
        .settle_grant(Settlement {
            principal: principal.clone(),
            key: SettlementKey::OncePerCall,
            response_id: call_id,
            actual_usd: usd,
            window: BudgetWindow::Total,
            now_ms: 2_000,
        })
        .await
        .expect("and settles it");
}

async fn committed(ledger: &dyn SpendLedger, principal: &Principal) -> f64 {
    ledger
        .balance(BalanceQuery {
            principal: principal.clone(),
            terms: terms(),
            now_ms: 2_000,
        })
        .await
        .expect("a balance reads")
        .committed_usd
}

fn serving_of(backends: &Backends) -> &std::sync::Arc<dyn SpendLedger> {
    match backends {
        Backends::Shared { spend, .. } => spend,
        _ => panic!("naming a Redis must select the shared arm"),
    }
}

/// **The claim.** An evaluation call's spend lands where another node's
/// evaluation ledger will find it, and nowhere the serving ledger can see.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn evaluation_spend_is_separate_from_serving_spend_and_shared_between_nodes() {
    let url = url_from_env();
    let namespace = unique_namespace("");
    let backends = open(Some(&url), &namespace)
        .await
        .expect("the Redis named by the test variable must be reachable");
    let principal = fresh_principal("user_evaluation_ledger_boot");

    spend(
        backends.evaluation_spend().as_ref(),
        &principal,
        "eval_1",
        0.25,
    )
    .await;

    assert_eq!(
        committed(serving_of(&backends).as_ref(), &principal).await,
        0.0,
        "an evaluation call committed against the serving ledger would start \
         refusing this project's turns"
    );
    assert_eq!(
        committed(backends.evaluation_spend().as_ref(), &principal).await,
        0.25,
        "and the evaluation ledger holds it"
    );

    // A second node: a fresh handle on the same namespace and purpose, which is
    // what makes the family *shared* rather than merely separate.
    let second_node = RedisSpendLedger::connect_for(&url, namespace, SpendPurpose::Evaluation)
        .await
        .expect("a second handle on the evaluation purpose");
    assert_eq!(
        committed(&second_node, &principal).await,
        0.25,
        "another node must see this deployment's evaluation spend, or the \
         ceiling is per process and the configuration says otherwise"
    );
}

/// **The collision, refuted against a real backend.**
///
/// Two deployments an operator may legitimately name `tenant` and `tenant-eval`.
/// The first's *evaluation* ledger must not be the second's *serving* ledger —
/// which is exactly what appending `-eval` to a namespace produced.
///
/// The control is in the same test and is what makes it about the collision
/// rather than about Redis being empty: the first deployment's own evaluation
/// ledger does see the charge, and the second's serving ledger sees its own.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn one_deployments_evaluation_ledger_is_not_another_deployments_serving_ledger() {
    let url = url_from_env();
    let tenant = unique_namespace("");
    let tenant_eval = KeyNamespace::new(format!("{}-eval", tenant.as_str()))
        .expect("a legal namespace an operator may choose");

    let first = open(Some(&url), &tenant).await.expect("deployment one");
    let second = open(Some(&url), &tenant_eval)
        .await
        .expect("deployment two, legitimately named");

    // One principal, because the collision is about *keys* and a different
    // project id would hide it behind the hash tag.
    let principal = fresh_principal("user_two_deployments");

    spend(first.evaluation_spend().as_ref(), &principal, "eval_a", 7.0).await;

    assert_eq!(
        committed(serving_of(&second).as_ref(), &principal).await,
        0.0,
        "deployment `tenant`'s evaluation spend reached deployment \
         `tenant-eval`'s serving ledger: two tenants are sharing one counter"
    );

    // CONTROLS. The charge is somewhere, and the second deployment's own
    // serving ledger still works — so the zero above is a boundary rather than
    // a Redis that answered nothing.
    assert_eq!(
        committed(first.evaluation_spend().as_ref(), &principal).await,
        7.0,
        "the charge must be in the ledger that took it"
    );
    spend(serving_of(&second).as_ref(), &principal, "serve_b", 3.0).await;
    assert_eq!(
        committed(serving_of(&second).as_ref(), &principal).await,
        3.0
    );
    assert_eq!(
        committed(first.evaluation_spend().as_ref(), &principal).await,
        7.0,
        "and neither deployment's spend moved the other's"
    );
    assert_eq!(
        committed(serving_of(&first).as_ref(), &principal).await,
        0.0,
        "nor did any of it reach deployment one's serving ledger"
    );
}

/// The per-process arm, for completeness: two ledgers with no keys at all are
/// separate because they are separate values.
#[tokio::test]
async fn the_per_process_arm_gives_two_distinct_ledgers() {
    let namespace = resolve_namespace(None).expect("the default");
    let backends = open(None, &namespace).await.expect("no Redis needed");
    let principal = fresh_principal("user_per_process");

    spend(
        backends.evaluation_spend().as_ref(),
        &principal,
        "eval_1",
        5.0,
    )
    .await;

    let serving = match &backends {
        Backends::PerProcess { spend, .. } => spend,
        _ => panic!("the per-process arm was expected"),
    };
    assert_eq!(committed(serving.as_ref(), &principal).await, 0.0);
    assert_eq!(
        committed(backends.evaluation_spend().as_ref(), &principal).await,
        5.0
    );
}
