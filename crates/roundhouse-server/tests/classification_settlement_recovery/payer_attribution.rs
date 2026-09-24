// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Claim 5: a repaired charge lands on the session's own payer and its own
//! budget window, never a live account's.

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
    seed_unconfirmed_settlement(&store, &lease, "eval_tenant_payer", 1, 0.05).await;
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
    seed_unconfirmed_settlement(&store, &lease, "eval_no_payer", 1, 0.05).await;
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

/// **Production repairs under the window the original intent recorded, and
/// this test is a passing regression pin of that -- not evidence that a
/// "live window" repair is needed.**
///
/// The requirements this stage inherited are explicit that the recorded
/// window is the contract to preserve: `TypeSafeShadow::repair_settlement`
/// settles with `window: settlement.window`, the amount the intent carried at
/// call time, on the documented rule that the live rate card and a repair's
/// own window are both out of scope for the same reason ("a repaired charge
/// that disagreed with the charge it replaced is drift nobody can see without
/// reading both"). This test drives that production code path unchanged and
/// it passes: the $5 committed under `Total` after the operator switched
/// `classify.budget.window` survives the Monthly-window repair that follows
/// it.
///
/// **Why it survives is a fact about this fixture's touch order, not a
/// general property of replaying a stale window.**
/// `ProjectAccount::settle_time` rolls `committed_usd` to zero only when the
/// window it is handed computes a start strictly later than
/// `window_started_ms`'s current high-water mark -- and the classifying
/// turn's own grant, made under the *original* Monthly config before the
/// operator ever switched anything, is the first ledger touch this account
/// ever sees. That call already advances `window_started_ms` to the current
/// month. The later Total-mode $5 leaves it there (`Total` always computes a
/// start of `0`), so when the repair replays `Monthly` in the same calendar
/// month, `window_start_ms(Monthly, now)` equals the mark already set and
/// nothing resets. A account whose *first-ever* ledger touch were the Monthly
/// repair itself -- e.g. one that had only ever seen `Total`-window spend
/// before this repair -- would roll: see
/// [`open_question_a_repair_can_still_roll_an_account_whose_first_ledger_touch_it_is`],
/// a ledger-level demonstration of exactly that, reported as an open
/// question this stage does not resolve. Nothing here changes production.
#[tokio::test]
async fn a_repair_does_not_roll_a_live_account_onto_the_window_its_intent_recorded() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    // The call is made, and its intent recorded, under a *monthly* window.
    let monthly = monthly_config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_window_drift");
    let principal = Principal::default_open();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &monthly, &session).await;

    // The operator switches the evaluation budget to a total window, and the
    // project accrues spend under it.
    let total = config(&base_url);
    let terms = total.budget_terms();
    ledger
        .commit_unrelated(&principal, terms.budget.window, 5.0)
        .await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        5.0,
        "the premise: the project has committed spend in the window that is \
         open now"
    );

    let restarted = deployment(&store, &ledger, &total).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        5.0 + usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed,
        5.0 + usd,
        "the $5 committed under Total survives this repair's Monthly settle"
    );
}

/// **Open question, not a production defect this stage resolves: a repair
/// can still roll an account whose first-ever ledger touch it is.**
///
/// Unlike [`a_repair_does_not_roll_a_live_account_onto_the_window_its_intent_recorded`],
/// nothing here touches this account under `Monthly` before the repair does.
/// `Total`-mode spend lands first -- `window_start_ms(Total, _)` is always
/// `0`, so it leaves `window_started_ms` at its initial `0` -- and the
/// Monthly-window settle that follows is the account's first look at a
/// nonzero window start. `ProjectAccount::settle_time` reads that as the
/// window having rolled and zeroes `committed_usd` before applying the
/// settle.
///
/// A production sequence that reaches this: a project whose evaluation
/// spend has only ever been committed under `Total` (or under `Monthly` in
/// an earlier calendar month whose watermark a `Total`-only stretch since
/// then never revisited) gets its first `Monthly`-window repair. This is a
/// ledger-level demonstration to make that reachable and named, not an
/// engine-level reproduction and not a fix -- both the engine path and the
/// right resolution (window-mode changes are an operator action already
/// outside a repair's remit; whether repair should carry a mode marker, or
/// whether this is simply the existing window-change contract working as
/// specified) are for whoever picks this up next to decide.
#[tokio::test]
async fn open_question_a_repair_can_still_roll_an_account_whose_first_ledger_touch_it_is() {
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

    // The account's first-ever Monthly settle -- what a repair whose intent
    // recorded a Monthly window would send, if this were its first touch. A
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
