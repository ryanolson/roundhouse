// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Classification settlement recovery through three fresh Redis backend connections.
//! Faults are injected around real settlement calls within one test process.
//! Set ROUNDHOUSE_TEST_REDIS_URL and use --include-ignored to run these cases.

use roundhouse_server::Backends;
use roundhouse_server::shared_backend::open;
use roundhouse_store_redis::test_support::url_from_env;
use roundhouse_store_redis::{KeyNamespace, RedisSessionStore};

use super::*;

/// Use a fresh namespace so earlier test charges cannot satisfy these assertions.
fn unique_namespace() -> KeyNamespace {
    let id = uuid::Uuid::new_v4().simple().to_string();
    KeyNamespace::new(format!("clsrecov{}", &id[..16])).expect("a legal namespace")
}

fn store_of(backends: &Backends) -> Arc<RedisSessionStore> {
    match backends {
        Backends::Shared { store, .. } => Arc::clone(store),
        Backends::PerProcess { .. } => panic!("naming a Redis must select the shared arm"),
    }
}

/// Check the serving ledger from the same backend configuration for unintended charges.
fn serving_of(backends: &Backends) -> Arc<dyn SpendLedger> {
    match backends {
        Backends::Shared { spend, .. } => Arc::clone(spend),
        Backends::PerProcess { .. } => panic!("naming a Redis must select the shared arm"),
    }
}

async fn balance(ledger: &dyn SpendLedger, principal: &Principal, terms: &BudgetTerms) -> Balance {
    ledger
        .balance(BalanceQuery {
            principal: principal.clone(),
            terms: terms.clone(),
            now_ms: roundhouse_core::now_ms(),
        })
        .await
        .expect("the real ledger answers")
}

/// The durable call identity and accounting expected after reopening.
struct RecoveredCall<'a> {
    session: &'a SessionId,
    principal: &'a Principal,
    terms: &'a BudgetTerms,
    call_id: &'a ResponseId,
    usd: f64,
}

/// Read the completed repair through fresh backends and serve another local-only turn.
async fn reopened_after_recovery(
    url: &str,
    namespace: &KeyNamespace,
    config: &ClassifyConfig,
    call: RecoveredCall<'_>,
    upstream: &ClassifierUpstream,
) {
    let backends = open(Some(url), namespace)
        .await
        .expect("reopening a third time under the same namespace");
    let store = store_of(&backends);
    let eval = Arc::clone(backends.evaluation_spend());
    let deployment = deployment_over(&store, Arc::clone(&eval), config).await;

    deployment.turn(call.session, "t5", "keep going").await;

    assert_eq!(
        intents(&store, call.session).await.len(),
        1,
        "one durable classification intent, read through a third connection"
    );
    let found = repairs(&store, call.session).await;
    assert_eq!(found.len(), 1, "one matching repair, read the same way");
    assert!(found[0]["record"]["call_id"] == call.call_id.to_string());

    let final_balance = balance(eval.as_ref(), call.principal, call.terms).await;
    assert_eq!(
        final_balance.committed_usd, call.usd,
        "the original committed amount"
    );
    assert_eq!(final_balance.held_usd, 0.0, "zero held");

    let serving = serving_of(&backends);
    let final_serving = balance(serving.as_ref(), call.principal, call.terms).await;
    assert_eq!(
        final_serving.committed_usd, 0.0,
        "the serving ledger stays untouched"
    );
    assert_eq!(final_serving.held_usd, 0.0);

    assert_eq!(upstream.count(), 1, "still one classifier HTTP call");
    deployment.runtime.shutdown().await;
}

/// Refuse every settlement attempt so automatic repair cannot resolve the failure before reopening.
struct PoisonedLedger {
    inner: Arc<dyn SpendLedger>,
    mode: FailMode,
}

impl PoisonedLedger {
    fn new(inner: Arc<dyn SpendLedger>, mode: FailMode) -> Arc<Self> {
        Arc::new(Self { inner, mode })
    }
}

#[async_trait]
impl SpendLedger for PoisonedLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        match self.mode {
            FailMode::BeforeApply => Err(SpendError::Backend(anyhow::anyhow!(
                "simulated settlement failure before the real ledger applied anything"
            ))),
            FailMode::AfterApply => {
                self.inner.settle_grant(settlement).await?;
                Err(SpendError::Backend(anyhow::anyhow!(
                    "simulated acknowledgement lost after the real ledger applied it"
                )))
            }
            FailMode::Never => self.inner.settle_grant(settlement).await,
        }
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.inner.balance(query).await
    }
}

/// **The before-apply claim, through a real Redis session log and a real
/// Redis evaluation ledger.**
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_settlement_that_failed_before_application_recovers_after_a_restart_through_real_redis() {
    let url = url_from_env();
    let namespace = unique_namespace();
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let session = SessionId::new("sess_redis_before_apply");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let backends = open(Some(&url), &namespace)
        .await
        .expect("the test redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let store = store_of(&backends);
    let real_eval = Arc::clone(backends.evaluation_spend());
    let real_serving = serving_of(&backends);
    let serving_before = balance(real_serving.as_ref(), &principal, &terms).await;

    let poisoned = PoisonedLedger::new(Arc::clone(&real_eval), FailMode::BeforeApply);
    let first = deployment_over(
        &store,
        Arc::clone(&poisoned) as Arc<dyn SpendLedger>,
        &config,
    )
    .await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;
    first.turn(&session, "t2", "now add a test").await;

    assert_eq!(
        upstream.count(),
        1,
        "the classifier's HTTP endpoint answered once"
    );
    let (call_id, usd) = recorded_measured(&store, &session).await;
    assert_eq!(
        intents(&store, &session).await.len(),
        1,
        "one durable classification intent"
    );
    assert!(
        results(&store, &session).await[0]
            .1
            .outcome
            .committed_usd()
            .is_none(),
        "the premise: nothing confirms this charge yet"
    );
    assert!(
        repairs(&store, &session).await.is_empty(),
        "the poisoned adapter must still be refusing every attempt -- the \
         delivery turn's own automatic repair must not have healed the \
         premise before the intended restart"
    );
    assert_eq!(
        balance(real_eval.as_ref(), &principal, &terms)
            .await
            .committed_usd,
        0.0,
        "the premise on the real ledger: a before-apply failure left it untouched"
    );

    first.runtime.shutdown().await;
    drop(first);
    drop(poisoned);
    drop(real_eval);
    drop(real_serving);
    drop(store);
    drop(backends);

    let restarted_backends = open(Some(&url), &namespace)
        .await
        .expect("reconnecting to the same redis and namespace");
    let restarted_store = store_of(&restarted_backends);
    let fresh_eval = Arc::clone(restarted_backends.evaluation_spend());
    let restarted = deployment_over(&restarted_store, Arc::clone(&fresh_eval), &config).await;

    let found = drive_until_repaired(&restarted, &restarted_store, &session, &["t3", "t4"]).await;
    assert_eq!(
        found.len(),
        1,
        "the successor commits exactly one durable repair acknowledgement"
    );
    assert!(found[0]["record"]["call_id"] == call_id.to_string());
    assert!(
        found[0]["record"]["applied"] == true,
        "a before-apply repair is this identity's first real application"
    );

    let recovered = balance(fresh_eval.as_ref(), &principal, &terms).await;
    assert_eq!(
        recovered.committed_usd, usd,
        "the original recorded price, recovered from the durable record"
    );
    assert_eq!(
        recovered.held_usd, 0.0,
        "the hold is released once the repair applies"
    );
    assert_eq!(
        upstream.count(),
        1,
        "recovered without buying the answer again"
    );
    assert_eq!(
        intents(&restarted_store, &session).await.len(),
        1,
        "no new classification intent across recovery"
    );

    let fresh_serving = serving_of(&restarted_backends);
    let serving_after = balance(fresh_serving.as_ref(), &principal, &terms).await;
    assert_eq!(serving_before.committed_usd, 0.0);
    assert_eq!(serving_before.held_usd, 0.0);
    assert_eq!(
        serving_after.committed_usd, 0.0,
        "the serving ledger's keys were never touched by the evaluation repair"
    );
    assert_eq!(serving_after.held_usd, 0.0);

    // Read through fresh backend handles after releasing the repaired deployment.
    restarted.runtime.shutdown().await;
    drop(restarted);
    drop(fresh_eval);
    drop(fresh_serving);
    drop(restarted_store);
    drop(restarted_backends);

    let call = RecoveredCall {
        session: &session,
        principal: &principal,
        terms: &terms,
        call_id: &call_id,
        usd,
    };
    reopened_after_recovery(&url, &namespace, &config, call, &upstream).await;
}

/// **The after-apply claim, through a real Redis session log and a real
/// Redis evaluation ledger.**
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_lost_acknowledgement_after_application_is_resolved_without_charging_twice_through_real_redis()
 {
    let url = url_from_env();
    let namespace = unique_namespace();
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let session = SessionId::new("sess_redis_lost_ack");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let backends = open(Some(&url), &namespace)
        .await
        .expect("the test redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let store = store_of(&backends);
    let real_eval = Arc::clone(backends.evaluation_spend());
    let real_serving = serving_of(&backends);
    let serving_before = balance(real_serving.as_ref(), &principal, &terms).await;

    let poisoned = PoisonedLedger::new(Arc::clone(&real_eval), FailMode::AfterApply);
    let first = deployment_over(
        &store,
        Arc::clone(&poisoned) as Arc<dyn SpendLedger>,
        &config,
    )
    .await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;
    first.turn(&session, "t2", "now add a test").await;

    assert_eq!(
        upstream.count(),
        1,
        "the classifier's HTTP endpoint answered once"
    );
    let (call_id, usd) = recorded_measured(&store, &session).await;
    assert_eq!(
        intents(&store, &session).await.len(),
        1,
        "one durable classification intent"
    );
    assert!(
        results(&store, &session).await[0]
            .1
            .outcome
            .committed_usd()
            .is_none(),
        "the premise: nothing confirms this charge yet, though the real \
         ledger already has it"
    );
    assert!(
        repairs(&store, &session).await.is_empty(),
        "the poisoned adapter must still be refusing every attempt -- the \
         delivery turn's own automatic repair must not have healed the \
         premise before the intended restart"
    );
    assert_eq!(
        balance(real_eval.as_ref(), &principal, &terms)
            .await
            .committed_usd,
        usd,
        "the premise on the real ledger: an after-apply failure already \
         committed the charge, and this process never learned it"
    );

    first.runtime.shutdown().await;
    drop(first);
    drop(poisoned);
    drop(real_eval);
    drop(real_serving);
    drop(store);
    drop(backends);

    let restarted_backends = open(Some(&url), &namespace)
        .await
        .expect("reconnecting to the same redis and namespace");
    let restarted_store = store_of(&restarted_backends);
    let fresh_eval = Arc::clone(restarted_backends.evaluation_spend());
    let restarted = deployment_over(&restarted_store, Arc::clone(&fresh_eval), &config).await;

    let found = drive_until_repaired(&restarted, &restarted_store, &session, &["t3", "t4"]).await;
    assert_eq!(
        found.len(),
        1,
        "the unconfirmed settlement is acknowledged exactly once in the log"
    );
    assert!(found[0]["record"]["call_id"] == call_id.to_string());
    assert!(
        found[0]["record"]["applied"] == false,
        "the real ledger already had this call -- a successful \
         acknowledgement, not a fresh application"
    );

    let recovered = balance(fresh_eval.as_ref(), &principal, &terms).await;
    assert_eq!(
        recovered.committed_usd, usd,
        "one effective charge, not two"
    );
    assert_eq!(
        recovered.held_usd, 0.0,
        "the hold was released when the charge first applied"
    );
    assert_eq!(upstream.count(), 1, "and no second purchase");
    assert_eq!(
        intents(&restarted_store, &session).await.len(),
        1,
        "no new classification intent across recovery"
    );

    let fresh_serving = serving_of(&restarted_backends);
    let serving_after = balance(fresh_serving.as_ref(), &principal, &terms).await;
    assert_eq!(serving_before.committed_usd, 0.0);
    assert_eq!(serving_before.held_usd, 0.0);
    assert_eq!(
        serving_after.committed_usd, 0.0,
        "the serving ledger's keys were never touched by the evaluation repair"
    );
    assert_eq!(serving_after.held_usd, 0.0);

    // Read through fresh backend handles after releasing the repaired deployment.
    restarted.runtime.shutdown().await;
    drop(restarted);
    drop(fresh_eval);
    drop(fresh_serving);
    drop(restarted_store);
    drop(restarted_backends);

    let call = RecoveredCall {
        session: &session,
        principal: &principal,
        terms: &terms,
        call_id: &call_id,
        usd,
    };
    reopened_after_recovery(&url, &namespace, &config, call, &upstream).await;
}
