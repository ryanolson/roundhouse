// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Claim 5: a repaired charge lands on the session's own payer, under the
//! budget window configured when the repair runs.

use super::*;

// ---------------------------------------------- payer attribution (claim 5)

/// **The positive control for payer attribution: a configured tenant's own
/// money, recovered after a restart, lands on that tenant.**
///
/// The two tests beside this one are both negatives -- a stranger's principal
/// is not used, and a log with no principal is refused -- and a repair path
/// that simply never charged anyone would satisfy both. This one drives the
/// supported shape end to end: a session whose log records
/// `acme/ada` as its payer, turns served under that same principal (which is
/// what every supported `ControlPlane` produces for one session), and the
/// recovered charge on `acme/ada`'s own account rather than on the open
/// default this suite's other fixtures use.
#[tokio::test]
async fn a_configured_tenants_charge_is_recovered_onto_its_own_account() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("acme/ada/sess_tenant_payer");
    let payer = Principal::new("acme", "ada");
    let terms = config.budget_terms();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_tenant_payer_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(payer.clone()),
                arm: None,
            }],
            None,
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(
        &store,
        &lease,
        "eval_tenant_payer",
        1,
        0.05,
        BudgetWindow::Total,
    )
    .await;
    store.release_lease(&lease).await.unwrap();

    let restarted =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    let admission = Admission {
        principal: payer.clone(),
        ..local_only()
    };
    for turn in ["t1", "t2", "t3"] {
        restarted
            .turn_as(&session, turn, "keep going", &admission)
            .await;
    }

    assert_eq!(
        ledger.committed_usd(&payer, &terms).await,
        0.05,
        "the recovered charge is on the tenant the log named"
    );
    assert_eq!(
        ledger
            .committed_usd(&Principal::default_open(), &terms)
            .await,
        0.0,
        "and nothing landed on the deployment-wide default account, which is \
         what a repair that ignored the recorded payer would reach for"
    );
    assert_eq!(
        repairs(&store, &session).await.len(),
        1,
        "one durable acknowledgement, on a supported control-plane shape"
    );
    assert_eq!(upstream.count(), 0, "and no classifier call, ever");
}

/// **A session whose log names no payer is never repaired -- not even under
/// a later turn carrying a perfectly good admission principal.**
///
/// `a_repaired_charge_is_attributed_to_the_sessions_own_payer` (above) proves
/// source-of-payer with an out-of-contract fixture -- a later turn under a
/// *different* principal, which no supported control plane can produce for
/// one session. This test proves the same source-of-payer property through a
/// state a supported deployment's own log can genuinely hold:
/// `SessionCreated { principal: None, .. }`, the "log older than tenancy"
/// case `Session::record_created`'s own doc comment names, reachable by any
/// log written before principals existed. `Engine::repair_classification_settlements`
/// reads `session.state().principal()` and refuses outright on `None`
/// (engine.rs) rather than falling back to the live turn's admission -- which
/// is exactly what this drives: an admission whose principal actually would
/// receive money if the fallback existed.
#[tokio::test]
async fn a_session_with_no_recorded_payer_is_never_repaired() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_no_payer");
    let live_turn_principal = Principal::new("someone", "would_be_charged");
    let terms = config.budget_terms();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_no_payer_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: None,
                arm: None,
            }],
            None,
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(
        &store,
        &lease,
        "eval_no_payer",
        1,
        0.05,
        BudgetWindow::Total,
    )
    .await;
    store.release_lease(&lease).await.unwrap();

    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    let admission = Admission {
        principal: live_turn_principal.clone(),
        ..local_only()
    };
    for turn in ["t1", "t2", "t3"] {
        deployment
            .turn_as(&session, turn, "keep going", &admission)
            .await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        repairs(&store, &session).await.is_empty(),
        "no repair may run at all -- there is no supported payer to attribute \
         one to"
    );
    assert_eq!(
        ledger.committed_usd(&live_turn_principal, &terms).await,
        0.0,
        "and specifically not on the live turn's own principal, which is the \
         mistake a fallback to the admission would make"
    );
    assert_eq!(ledger.settle_calls(), 0, "the ledger is never even asked");
    assert_eq!(upstream.count(), 0, "and no classifier call, ever");
}

/// **The other harness control.** A local-only target must not be what silences
/// the classifier in these fixtures.
///
/// Every test above asserts `upstream.count() == 1` after several later turns.
/// That is only evidence about *recovery* if those later turns would otherwise
/// have been classified — so this pins that the rig's first turn does reach a
/// frontier target and does produce exactly one intent.
#[tokio::test]
async fn control_the_rig_classifies_its_first_turn_through_a_frontier_target() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_rig_control");

    let first = deployment(&store, &ledger, &config).await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;

    let intents: Vec<_> = events(&store, &session)
        .await
        .into_iter()
        .filter(|event| matches!(event.kind, SessionEventKind::ClassificationRequested { .. }))
        .collect();
    assert_eq!(intents.len(), 1, "exactly one durable intent");
    assert_eq!(upstream.count(), 1, "and exactly one purchase");

    let decisions: Vec<Target> = events(&store, &session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.chosen),
            _ => None,
        })
        .collect();
    assert!(
        decisions.iter().any(|target| !target.is_local()),
        "the classified turn reached a frontier target, which is what permits \
         the call at all"
    );
}

/// **A repair after a window-mode change settles under the window configured
/// now, so it cannot reset spend committed under that window.**
///
/// The account key is per project, not per window mode, and both ledgers roll
/// an account under whatever mode a settle names *before* they check whether
/// the call was already settled. A repair that replayed the mode its intent
/// recorded would therefore roll a live `Total` account onto a `Monthly`
/// boundary the moment a month had passed, deleting the lifetime balance --
/// and it would do so even when the ledger then answers `applied: false`,
/// because the roll comes first. Both sides of that are driven here:
///
/// - `BeforeApply`: the repair is the call's first charge, so it adds to the
///   balance and must add only its own amount.
/// - `AfterApply`: the call was already charged in the earlier month, so the
///   repair is a duplicate and must leave the balance exactly where it was.
///
/// **The log is seeded, not produced by a classifying turn.** Every turn ends
/// by driving repairs, so a first deployment that made the call would leave
/// its own repair worker in flight under its own `Monthly` configuration, and
/// that worker -- not the restarted process -- could be the settle that rolls
/// the account. A real restart does not carry the old process's tasks over;
/// seeding is what makes the restarted `Total` process the only repairer.
#[tokio::test]
async fn a_repair_after_a_window_mode_change_keeps_the_live_windows_committed_spend() {
    for mode in [FailMode::BeforeApply, FailMode::AfterApply] {
        let (base_url, _upstream) = classifier_upstream(ANSWER).await;
        let total = config(&base_url);
        let terms = total.budget_terms();
        let monthly_terms = monthly_config(&base_url).budget_terms();
        let store = Arc::new(MemoryStore::new());
        let ledger = RiggedLedger::new(mode);
        let session = SessionId::new("sess_window_mode_change");
        let principal = Principal::default_open();
        let call_id = ResponseId::new("eval_window_mode_change");
        let usd = 0.05;

        // The log a `Monthly`-configured process left: a call whose result
        // says nobody acknowledged its settle.
        store.create_session(&session, "affinity").await.unwrap();
        let lease = store
            .acquire_lease(&session, "node_window_mode_setup", 60_000)
            .await
            .expect("a lease read")
            .expect("an unheld session");
        store
            .append_events(
                &lease,
                vec![SessionEventKind::SessionCreated {
                    model_policy: "affinity".into(),
                    principal: Some(principal.clone()),
                    arm: None,
                }],
                None,
            )
            .await
            .unwrap();
        seed_unconfirmed_settlement(
            &store,
            &lease,
            call_id.as_str(),
            1,
            usd,
            BudgetWindow::Monthly,
        )
        .await;
        store.release_lease(&lease).await.unwrap();

        // And the ledger side of that call, this month, under `Monthly`: the
        // settle whose acknowledgement never came back (applied or not, per
        // `mode`), and the account read that marks this month as the one its
        // balance belongs to.
        let original = ledger
            .settle_grant(Settlement {
                principal: principal.clone(),
                key: SettlementKey::OncePerCall,
                response_id: call_id.clone(),
                actual_usd: usd,
                window: monthly_terms.budget.window,
                now_ms: roundhouse_core::now_ms(),
            })
            .await;
        assert!(original.is_err(), "{mode:?}: the premise is a lost settle");
        ledger.committed_usd(&principal, &monthly_terms).await;

        // The operator switches to `Total`, and the project spends under it.
        ledger
            .commit_unrelated(&principal, terms.budget.window, 5.0)
            .await;
        let before = ledger.committed_usd(&principal, &terms).await;

        // Two months on, a restarted `Total` process drives the repair.
        ledger.skew(62 * 24 * 60 * 60 * 1_000);
        let restarted = deployment(&store, &ledger, &total).await;
        let repaired =
            drive_until_repaired_for(&restarted, &store, &session, &call_id, &["t1", "t2", "t3"])
                .await;
        assert_eq!(repaired, 1, "{mode:?}: the repair must be acknowledged");

        let expected = match mode {
            FailMode::BeforeApply => before + usd,
            _ => before,
        };
        let committed = ledger.committed_usd(&principal, &terms).await;
        assert!(
            (committed - expected).abs() < 1e-9,
            "{mode:?}: the Total window held {before} before the repair and \
             must hold {expected} after it, but holds {committed} -- a repair \
             that settles under the Monthly mode its intent recorded rolls the \
             account onto a month boundary and deletes the lifetime balance"
        );
        if mode == FailMode::AfterApply {
            assert!(
                ledger.deduplicated(call_id.as_str()),
                "the control: the AfterApply repair really was a duplicate, \
                 so any change to the balance came from the roll alone"
            );
        }
    }
}

/// **The control for the test above: the ledger itself rolls an account under
/// whatever window a settle names.**
///
/// `Total`-mode spend lands first -- `window_start_ms(Total, _)` is always
/// `0`, so it leaves `window_started_ms` at its initial `0` -- and a
/// `Monthly` settle that follows is the account's first look at a nonzero
/// window start. `ProjectAccount::settle_time` reads that as the window
/// having rolled and zeroes `committed_usd` before applying the settle.
///
/// This is why a repair must name the live window rather than the one its
/// intent recorded, and it is what keeps
/// [`a_repair_after_a_window_mode_change_keeps_the_live_windows_committed_spend`]
/// from being tautological: if the ledger stopped rolling on a named window,
/// that test would pass whichever window the repair named, and this one
/// would fail first.
#[tokio::test]
async fn the_ledger_rolls_an_account_under_whatever_window_a_settle_names() {
    let ledger = RiggedLedger::new(FailMode::Never);
    let principal = Principal::default_open();
    let total_terms = config("http://127.0.0.1:1").budget_terms();
    let monthly_terms = monthly_config("http://127.0.0.1:1").budget_terms();

    ledger
        .commit_unrelated(&principal, total_terms.budget.window, 5.0)
        .await;
    assert_eq!(
        ledger.committed_usd(&principal, &total_terms).await,
        5.0,
        "the premise: this account's only ledger touch so far is under Total"
    );

    // The account's first-ever Monthly settle -- what a repair that replayed
    // a recorded Monthly window would send after a switch to Total. A
    // distinct response id: `commit_unrelated` always names the same one, and
    // `SettlementKey::OncePerCall` would read a second call under it as the
    // same settle repeated rather than a fresh one.
    ledger
        .settle_grant(Settlement {
            principal: principal.clone(),
            key: SettlementKey::OncePerCall,
            response_id: ResponseId::new("repair_like_monthly_call"),
            actual_usd: 0.02,
            window: monthly_terms.budget.window,
            now_ms: roundhouse_core::now_ms(),
        })
        .await
        .expect("an in-memory ledger settles");

    let after_total = ledger.committed_usd(&principal, &total_terms).await;
    let after_monthly = ledger.committed_usd(&principal, &monthly_terms).await;
    assert_eq!(
        after_monthly, 0.02,
        "the Monthly settle itself always lands"
    );
    assert_eq!(
        after_total, 0.02,
        "and it rolled the account first: read back under Total (which never \
         itself triggers a reset) the balance is only the 0.02 the Monthly \
         settle added, not 5.02 -- the $5 is gone, lost to a reset the \
         Monthly settle caused as a side effect of applying. This is the open \
         question, demonstrated at the ledger, not asserted as desired \
         behavior"
    );
}
