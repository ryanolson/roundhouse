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
//! A zero-dollar release must never trigger that same withhold at all --
//! the routine case behind the claim, not a ledger outage, and just as
//! untested by the shape above. [`settlement_repair::YieldingLedger`] is
//! what reproduces one without touching `typesafe_shadow` itself.
//!
//! Neither shape proves the withhold actually *lifts* once the debt it was
//! for is repaid, either: a mutation that withholds unconditionally would
//! still pass every assertion above it, since none of them ever asks the
//! classifier again after the repair lands. The last turn appended to
//! [`a_sessions_own_new_ticket_does_not_starve_its_owed_repair`] does.

use super::settlement_repair::{SettleOnceFailingLedger, YieldingLedger};
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
/// t1's call never gets an answer: the upstream hangs, so the client's own
/// `call_ttl_ms` deadline fires first and `send_and_settle` submits a release
/// at zero -- `EvaluationSpend::Unknown`. The settle that follows reuses that
/// same, already-elapsed deadline, and [`YieldingLedger`] forces its first
/// poll to return `Pending` -- exactly what a real network round trip does
/// for free -- so `settle_once`'s own `timeout_at` finds the clock already
/// past and answers `Unconfirmed`: a zero-dollar entry in
/// `unrepaired_settlements`, the routine case this test is about, not a
/// contrived one. t2 owes nothing and must still buy its own classification.
#[tokio::test]
async fn a_zero_dollar_release_does_not_withhold_the_next_turns_ticket() {
    use axum::Router;
    use axum::routing::post;
    use std::future::pending;

    // A classifier that never answers -- signals `arrived` the instant its
    // handler is invoked, then hangs forever, so t1's call is cut off by its
    // own deadline rather than by anything server-side.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let handler_arrived = Arc::clone(&arrived);
    let app = Router::new().route(
        "/systemone",
        post(move || {
            let arrived = Arc::clone(&handler_arrived);
            async move {
                arrived.notify_one();
                pending::<()>().await
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
        value["executor"]["call_ttl_ms"] = serde_json::json!(200);
    });
    let ledger = YieldingLedger::new();
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
    assert_eq!(
        parked.len(),
        1,
        "t1's call must complete by its own deadline, not by an answer"
    );
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
