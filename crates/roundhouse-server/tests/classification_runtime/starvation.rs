// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Self-starvation: a session's own new classification ticket must not
//! pre-empt that same session's owed repair.
//!
//! At one in-flight slot, `classification_before_turn`'s drain frees exactly
//! the one permit `classification_after_turn`'s repair loop would need. A
//! session that owes a repair withholds its own new ticket there instead of
//! spending that permit on a fresh purchase, so `classifier.capacity()` is
//! still free when the repair loop runs a few lines later.
//! [`settlement_repair::SettleOnceFailingLedger`] is what makes the first
//! settlement go unconfirmed without a real provider outage: this suite
//! reuses it rather than inventing a second failing ledger.
//!
//! A zero-dollar release must never trigger that same withhold at all, and
//! the shape above does not test that either. A zero-dollar release is what
//! a call the service refused with an error status submits; a call that may
//! have been billed books its estimate instead (2026-09-28 ruling 3), so the
//! zero-dollar fixtures here answer 529 rather than hang.
//! [`settlement_repair::SettleOnceFailingLedger`] leaves that release
//! unconfirmed the same way it does any other first settle.
//!
//! Neither shape proves the withhold actually *lifts* once the debt it was
//! for is repaid, either: a mutation that withholds unconditionally would
//! still pass every assertion above it, since none of them ever asks the
//! classifier again after the repair lands. The last turn appended to
//! [`a_sessions_own_new_ticket_does_not_starve_its_owed_repair`] does.

use super::settlement_repair::SettleOnceFailingLedger;
use super::*;

/// [`config`] at one in-flight slot -- so the permit a session's own new
/// ticket takes is the only one this classifier has, the shape the
/// self-starvation claim is about.
fn one_slot_config(base_url: &str) -> ClassifyConfig {
    classify_config(base_url, |value| {
        value["enabled"] = serde_json::json!(true);
        value["executor"]["max_in_flight"] = serde_json::json!(1);
    })
}

/// **A session's own new ticket must not starve that same session's owed
/// repair.** `t1`'s call settles unconfirmed. Every turn after it admits a
/// frontier target and, at one in-flight slot, drains `t1`'s parked result
/// on the way in -- which is exactly the turn that also discovers the
/// session now owes a repair. Several turns run rather than one, because the
/// claim is that this repeats *forever*, not just once.
#[tokio::test]
async fn a_sessions_own_new_ticket_does_not_starve_its_owed_repair() {
    let (base_url, upstream) = classifier_upstream().await;
    let classify = one_slot_config(&base_url);
    let ledger = SettleOnceFailingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_self_starvation");
    engine.create_session(&session).await.unwrap();

    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers")
        }
    };

    turn("t1", "fix the parser").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parked = runtime.ready(&session).await;
    assert_eq!(parked.len(), 1, "t1's classification completed and parked");
    let call_id = parked[0].record.call_id.to_string();
    drop(parked);
    assert_eq!(
        ledger.settle_calls(),
        1,
        "the first settle genuinely failed"
    );

    // Every one of these turns admits a frontier target, so a session that
    // still withholds its own ticket would repeat, on each of them, exactly
    // the sequence the claim is about: deliver, spend the freed permit on a
    // fresh ticket instead, and leave the repair loop with no capacity.
    // Bounded polling after each turn, not a fixed count of them: the
    // repair's own settle is what this loop is waiting on, since it lands on
    // whichever turn's repair worker happens to finish first.
    for (id, text) in [
        ("t2", "add a test"),
        ("t3", "one more"),
        ("t4", "and another"),
        ("t5", "keep going"),
    ] {
        turn(id, text).await;
        for _ in 0..300 {
            if ledger.applied_usd(&call_id).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    assert_eq!(
        ledger.applications(&call_id),
        1,
        "a second, successful settle_grant for t1's own call must eventually \
         arrive -- a session's own repeated new tickets must not hold the \
         one slot away from the repair it already owes, turn after turn"
    );

    // **The withhold must lift once the debt it was for is gone.** Neither
    // this test's loop above nor `settlement_repair.rs` would catch a
    // withhold that stuck permanently once the entry that justified it left
    // `unrepaired_settlements` -- so ask one more time, deliberately after
    // the repair is already confirmed applied, and check the classifier is
    // actually asked again under a call id nothing above used.
    let calls_before_resuming = upstream.count();
    turn("t6", "keep going still").await;
    for _ in 0..300 {
        if upstream.count() > calls_before_resuming {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        upstream.count() > calls_before_resuming,
        "t6 must buy its own classification once the repair it might once \
         have been withheld for is no longer owed"
    );
    let intents = intents_in(&*store, &session).await;
    assert!(
        intents
            .iter()
            .any(|intent| intent.call_id.to_string() != call_id),
        "t6's own intent must carry a call id distinct from t1's repaired one"
    );
}

/// **Control: two in-flight slots leave the repair loop a permit even
/// without withholding anything.** A session with slack left over after its
/// own new ticket can always find a free permit for the repair loop -- this
/// is the ordinary-capacity shape the self-starvation claim above is
/// distinct from, not a case that depends on withholding a ticket at all.
#[tokio::test]
async fn two_slots_leave_room_for_the_repair() {
    let (base_url, _upstream) = classifier_upstream().await;
    let classify = classify_config(&base_url, |value| {
        value["enabled"] = serde_json::json!(true);
        value["executor"]["max_in_flight"] = serde_json::json!(2);
    });
    let ledger = SettleOnceFailingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_two_slots_control");
    engine.create_session(&session).await.unwrap();

    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers")
        }
    };

    turn("t1", "fix the parser").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parked = runtime.ready(&session).await;
    assert_eq!(parked.len(), 1, "t1's classification completed and parked");
    let call_id = parked[0].record.call_id.to_string();
    drop(parked);

    turn("t2", "add a test").await;
    turn("t3", "one more").await;

    let mut recovered = None;
    for _ in 0..300 {
        recovered = ledger.applied_usd(&call_id);
        if recovered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        recovered.is_some(),
        "with slack left over after each turn's own ticket, the repair was \
         never starved of a permit"
    );
    assert_eq!(
        ledger.applications(&call_id),
        1,
        "settled exactly once, however many turns replayed the log after it"
    );
}

/// **A zero-dollar release must never withhold the next turn's own ticket.**
///
/// t1's call is refused: the upstream answers 529, so `send_and_settle`
/// submits a release at zero -- `EvaluationSpend::Unknown` with nothing
/// booked, because a service that declined the work billed nothing.
/// [`SettleOnceFailingLedger`] refuses that first settle, which leaves a
/// zero-dollar entry in `unrepaired_settlements`. t2 owes nothing and must
/// still buy its own classification.
#[tokio::test]
async fn a_zero_dollar_release_does_not_withhold_the_next_turns_ticket() {
    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;

    // A classifier that refuses every call -- and signals `arrived` the
    // instant its handler is invoked, so the test knows the call was made.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let handler_arrived = Arc::clone(&arrived);
    let app = Router::new().route(
        "/systemone",
        post(move || {
            let arrived = Arc::clone(&handler_arrived);
            async move {
                arrived.notify_one();
                Response::builder()
                    .status(529)
                    .body(Body::from(r#"{"error":"overloaded"}"#))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base_url = format!("http://{addr}");

    let classify = classify_config(&base_url, |value| {
        value["enabled"] = serde_json::json!(true);
    });
    let ledger = SettleOnceFailingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_zero_dollar_release");
    engine.create_session(&session).await.unwrap();

    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers")
        }
    };

    turn("t1", "fix the parser").await;
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .expect("t1's call must actually reach the upstream for this test to be about anything");
    for _ in 0..500 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parked = runtime.ready(&session).await;
    assert_eq!(parked.len(), 1, "t1's refused call completed and parked");
    let spend = parked[0]
        .record
        .outcome
        .spend()
        .expect("a spend was attempted");
    assert_eq!(
        spend.unconfirmed_settlement_usd(),
        Some(0.0),
        "the release must be zero-dollar and unconfirmed for this test to be \
         about the claim it names"
    );
    drop(parked);

    turn("t2", "add a test").await;
    let intents = intents_in(&*store, &session).await;
    assert_eq!(
        intents.len(),
        2,
        "t2 must still buy its own classification -- a zero-dollar release \
         owes nothing, so there is no repair for it to protect a permit for"
    );
}

/// How many durable `ClassificationRecorded` events this session's log holds
/// whose settlement is zero-dollar and unconfirmed -- the release a refused
/// call submits, left unacknowledged.
async fn zero_dollar_unconfirmed_results(store: &impl SessionStore, session: &SessionId) -> usize {
    store
        .read_events(session, 0, 1_000)
        .await
        .expect("a log reads")
        .into_iter()
        .filter(|event| {
            matches!(
                &event.kind,
                SessionEventKind::ClassificationRecorded { record }
                    if record
                        .outcome
                        .spend()
                        .and_then(|spend| spend.unconfirmed_settlement_usd())
                        == Some(0.0)
            )
        })
        .count()
}

/// **A real debt must not wait behind a wall of zero-dollar releases that
/// merely arrived first.**
///
/// At one in-flight slot, over a classifier that refuses its first three
/// calls with a 529 -- a status the service billed nothing for, so each
/// releases at zero -- and [`SettleOnceFailingLedger`], which fails every
/// call's first settle attempt, three zero-dollar unconfirmed entries
/// accumulate in the log, one per turn that delivers the previous call and
/// dispatches the next (`t1` dispatches the first; `t2` delivers it and
/// dispatches the second; and so on). The fourth call answers normally, and
/// the same ledger turns it into a genuine positive debt the same way it
/// turns any answered call's settle into one.
///
/// The turn that delivers the fourth call's result (`t5`) is also the turn
/// that first owes a real repair, so it withholds its own new ticket (the
/// self-starvation fix `a_sessions_own_new_ticket_does_not_starve_its_owed_repair`
/// covers) rather than spending the freed permit on a fifth purchase --
/// which is exactly what leaves that permit for the repair loop. Without
/// `repair_batch`'s positive-first ordering, that permit would go to the
/// oldest of the three zero-dollar entries instead, and the fourth call's
/// own debt would wait one more `max_in_flight`-sized turn for each
/// zero-dollar entry ahead of it -- four turns in this shape, not one.
#[tokio::test]
async fn a_positive_debt_is_repaired_ahead_of_zero_dollar_entries_that_arrived_first() {
    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;

    let calls = Arc::new(AtomicUsize::new(0));
    let arrived = Arc::new(tokio::sync::Notify::new());
    let handler_calls = Arc::clone(&calls);
    let handler_arrived = Arc::clone(&arrived);
    let app = Router::new().route(
        "/systemone",
        post(move || {
            let calls = Arc::clone(&handler_calls);
            let arrived = Arc::clone(&handler_arrived);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                arrived.notify_one();
                if n <= 3 {
                    Response::builder()
                        .status(529)
                        .body(Body::from(r#"{"error":"overloaded"}"#))
                        .unwrap()
                } else {
                    Response::new(Body::from(
                        roundhouse_server::test_support::classification::ANSWER,
                    ))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base_url = format!("http://{addr}");

    let classify = classify_config(&base_url, |value| {
        value["enabled"] = serde_json::json!(true);
        value["executor"]["max_in_flight"] = serde_json::json!(1);
    });
    let ledger = SettleOnceFailingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_repair_priority");
    engine.create_session(&session).await.unwrap();

    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers")
        }
    };

    /// Wait for the `want`th call to reach the upstream, then for the
    /// runtime to park at least `count` results for this session -- bounded
    /// on the arrival notification rather than a guessed sleep, and on the
    /// park count rather than the elapsed time.
    async fn await_call_then_parked(
        arrived: &tokio::sync::Notify,
        calls: &AtomicUsize,
        want: usize,
        runtime: &Arc<ClassificationRuntime<ByteTokenizer>>,
        session: &SessionId,
        count: usize,
    ) {
        while calls.load(Ordering::SeqCst) < want {
            tokio::time::timeout(Duration::from_secs(5), arrived.notified())
                .await
                .expect("the call must reach the upstream");
        }
        for _ in 0..600 {
            if runtime.ready(session).await.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("call {want} never parked a result");
    }

    turn("t1", "fix the parser").await;
    await_call_then_parked(&arrived, &calls, 1, &runtime, &session, 1).await;
    {
        let parked = runtime.ready(&session).await;
        assert_eq!(
            parked[0]
                .record
                .outcome
                .spend()
                .and_then(|spend| spend.unconfirmed_settlement_usd()),
            Some(0.0),
            "the first call must be refused and release at zero"
        );
    }

    turn("t2", "add a test").await;
    await_call_then_parked(&arrived, &calls, 2, &runtime, &session, 1).await;
    {
        let parked = runtime.ready(&session).await;
        assert_eq!(
            parked[0]
                .record
                .outcome
                .spend()
                .and_then(|spend| spend.unconfirmed_settlement_usd()),
            Some(0.0),
            "the second call must release at zero the same way"
        );
    }

    turn("t3", "one more").await;
    await_call_then_parked(&arrived, &calls, 3, &runtime, &session, 1).await;
    {
        let parked = runtime.ready(&session).await;
        assert_eq!(
            parked[0]
                .record
                .outcome
                .spend()
                .and_then(|spend| spend.unconfirmed_settlement_usd()),
            Some(0.0),
            "the third call must release at zero the same way"
        );
    }

    // t4 delivers the third zero-dollar result -- the accumulated premise
    // this test is about -- and dispatches the fourth call, the one this
    // classifier answers normally.
    turn("t4", "and another").await;
    assert_eq!(
        zero_dollar_unconfirmed_results(&*store, &session).await,
        3,
        "three zero-dollar entries must have accumulated before the debt \
         this test is about even exists"
    );

    let call4_id = {
        let mut found = None;
        for _ in 0..600 {
            let parked = runtime.ready(&session).await;
            if let Some(entry) = parked.first() {
                let spend = entry
                    .record
                    .outcome
                    .spend()
                    .expect("the fourth call answered, so a spend was attempted");
                assert_eq!(
                    spend.settled(),
                    roundhouse_core::classify::SettlementAck::Unconfirmed,
                    "SettleOnceFailingLedger fails every call's first settle"
                );
                assert!(
                    spend.unconfirmed_settlement_usd().unwrap_or(0.0) > 0.0,
                    "the fourth call answered normally and must carry a \
                     positive, unconfirmed debt: {spend:?}"
                );
                found = Some(entry.record.call_id.to_string());
                break;
            }
            drop(parked);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        found.expect("the fourth call must complete and park a result")
    };

    // t5 delivers the fourth call's result. Delivering it is what first
    // makes this session owe a real repair, so t5 withholds its own new
    // ticket rather than spending the just-freed permit on a fifth
    // purchase -- leaving that permit for the repair loop below to find.
    turn("t5", "keep going").await;
    let mut recovered = None;
    for _ in 0..300 {
        recovered = ledger.applied_usd(&call4_id);
        if recovered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        recovered.is_some(),
        "the fourth call's own debt must be repaired within a turn or two \
         of becoming one, not after every zero-dollar entry ahead of it has \
         first been drained one at a time"
    );
}
