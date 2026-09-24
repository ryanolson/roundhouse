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
    let (base_url, _upstream) = classifier_upstream().await;
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
